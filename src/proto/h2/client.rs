use futures_channel::{mpsc, oneshot};
use futures_util::future::{self, Either, FutureExt as _, TryFutureExt as _};
use futures_util::stream::StreamExt as _;
use h2::client::{Builder, SendRequest};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{bdp, decode_content_length, PipeToSendStream, SendBuf};
use crate::body::Payload;
use crate::common::{task, Exec, Future, Never, Pin, Poll};
use crate::headers;
use crate::proto::Dispatched;
use crate::{Body, Request, Response};

type ClientRx<B> = crate::client::dispatch::Receiver<Request<B>, Response<Body>>;

///// An mpsc channel is used to help notify the `Connection` task when *all*
///// other handles to it have been dropped, so that it can shutdown.
type ConnDropRef = mpsc::Sender<Never>;

///// A oneshot channel watches the `Connection` task, and when it completes,
///// the "dispatch" task will be notified and can shutdown sooner.
type ConnEof = oneshot::Receiver<Never>;

// Our defaults are chosen for the "majority" case, which usually are not
// resource constrained, and so the spec default of 64kb can be too limiting
// for performance.
const DEFAULT_CONN_WINDOW: u32 = 1024 * 1024 * 5; // 5mb
const DEFAULT_STREAM_WINDOW: u32 = 1024 * 1024 * 2; // 2mb

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) adaptive_window: bool,
    pub(crate) initial_conn_window_size: u32,
    pub(crate) initial_stream_window_size: u32,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            adaptive_window: false,
            initial_conn_window_size: DEFAULT_CONN_WINDOW,
            initial_stream_window_size: DEFAULT_STREAM_WINDOW,
        }
    }
}

pub(crate) async fn handshake<T, B>(
    io: T,
    req_rx: ClientRx<B>,
    config: &Config,
    exec: Exec,
) -> crate::Result<ClientTask<B>>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    B: Payload,
{
    let (h2_tx, mut conn) = Builder::default()
        .initial_window_size(config.initial_stream_window_size)
        .initial_connection_window_size(config.initial_conn_window_size)
        .enable_push(false)
        .handshake::<_, SendBuf<B::Data>>(io)
        .await
        .map_err(crate::Error::new_h2)?;

    // An mpsc channel is used entirely to detect when the
    // 'Client' has been dropped. This is to get around a bug
    // in h2 where dropping all SendRequests won't notify a
    // parked Connection.
    let (conn_drop_ref, rx) = mpsc::channel(1);
    let (cancel_tx, conn_eof) = oneshot::channel();

    let conn_drop_rx = rx.into_future().map(|(item, _rx)| {
        if let Some(never) = item {
            match never {}
        }
    });

    let sampler = if config.adaptive_window {
        let (sampler, mut estimator) =
            bdp::channel(conn.ping_pong().unwrap(), config.initial_stream_window_size);

        let conn = future::poll_fn(move |cx| {
            match estimator.poll_estimate(cx) {
                Poll::Ready(wnd) => {
                    conn.set_target_window_size(wnd);
                    conn.set_initial_window_size(wnd)?;
                }
                Poll::Pending => {}
            }

            Pin::new(&mut conn).poll(cx)
        });
        let conn = conn.map_err(|e| debug!("connection error: {}", e));

        exec.execute(conn_task(conn, conn_drop_rx, cancel_tx));
        sampler
    } else {
        let conn = conn.map_err(|e| debug!("connection error: {}", e));

        exec.execute(conn_task(conn, conn_drop_rx, cancel_tx));
        bdp::disabled()
    };

    Ok(ClientTask {
        bdp: sampler,
        conn_drop_ref,
        conn_eof,
        executor: exec,
        h2_tx,
        req_rx,
    })
}

async fn conn_task<C, D>(conn: C, drop_rx: D, cancel_tx: oneshot::Sender<Never>)
where
    C: Future + Unpin,
    D: Future<Output = ()> + Unpin,
{
    match future::select(conn, drop_rx).await {
        Either::Left(_) => {
            // ok or err, the `conn` has finished
        }
        Either::Right(((), conn)) => {
            // mpsc has been dropped, hopefully polling
            // the connection some more should start shutdown
            // and then close
            trace!("send_request dropped, starting conn shutdown");
            drop(cancel_tx);
            let _ = conn.await;
        }
    }
}

pub(crate) struct ClientTask<B>
where
    B: Payload,
{
    bdp: bdp::Sampler,
    conn_drop_ref: ConnDropRef,
    conn_eof: ConnEof,
    executor: Exec,
    h2_tx: SendRequest<SendBuf<B::Data>>,
    req_rx: ClientRx<B>,
}

impl<B> Future for ClientTask<B>
where
    B: Payload + 'static,
{
    type Output = crate::Result<Dispatched>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        loop {
            match ready!(self.h2_tx.poll_ready(cx)) {
                Ok(()) => (),
                Err(err) => {
                    return if err.reason() == Some(::h2::Reason::NO_ERROR) {
                        trace!("connection gracefully shutdown");
                        Poll::Ready(Ok(Dispatched::Shutdown))
                    } else {
                        Poll::Ready(Err(crate::Error::new_h2(err)))
                    };
                }
            };

            match Pin::new(&mut self.req_rx).poll_next(cx) {
                Poll::Ready(Some((req, cb))) => {
                    // check that future hasn't been canceled already
                    if cb.is_canceled() {
                        trace!("request callback is canceled");
                        continue;
                    }
                    let (head, body) = req.into_parts();
                    let mut req = ::http::Request::from_parts(head, ());
                    super::strip_connection_headers(req.headers_mut(), true);
                    if let Some(len) = body.size_hint().exact() {
                        if len != 0 || headers::method_has_defined_payload_semantics(req.method()) {
                            headers::set_content_length_if_missing(req.headers_mut(), len);
                        }
                    }
                    let eos = body.is_end_stream();
                    let (fut, body_tx) = match self.h2_tx.send_request(req, eos) {
                        Ok(ok) => ok,
                        Err(err) => {
                            debug!("client send request error: {}", err);
                            cb.send(Err((crate::Error::new_h2(err), None)));
                            continue;
                        }
                    };

                    if !eos {
                        let mut pipe = Box::pin(PipeToSendStream::new(body, body_tx)).map(|res| {
                            if let Err(e) = res {
                                debug!("client request body error: {}", e);
                            }
                        });

                        // eagerly see if the body pipe is ready and
                        // can thus skip allocating in the executor
                        match Pin::new(&mut pipe).poll(cx) {
                            Poll::Ready(_) => (),
                            Poll::Pending => {
                                let conn_drop_ref = self.conn_drop_ref.clone();
                                let pipe = pipe.map(move |x| {
                                    drop(conn_drop_ref);
                                    x
                                });
                                self.executor.execute(pipe);
                            }
                        }
                    }

                    let bdp = self.bdp.clone();
                    let fut = fut.map(move |result| match result {
                        Ok(res) => {
                            let content_length = decode_content_length(res.headers());
                            let res =
                                res.map(|stream| crate::Body::h2(stream, content_length, bdp));
                            Ok(res)
                        }
                        Err(err) => {
                            debug!("client response error: {}", err);
                            Err((crate::Error::new_h2(err), None))
                        }
                    });
                    self.executor.execute(cb.send_when(fut));
                    continue;
                }

                Poll::Ready(None) => {
                    trace!("client::dispatch::Sender dropped");
                    return Poll::Ready(Ok(Dispatched::Shutdown));
                }

                Poll::Pending => match ready!(Pin::new(&mut self.conn_eof).poll(cx)) {
                    Ok(never) => match never {},
                    Err(_conn_is_eof) => {
                        trace!("connection task is closed, closing dispatch task");
                        return Poll::Ready(Ok(Dispatched::Shutdown));
                    }
                },
            }
        }
    }
}
