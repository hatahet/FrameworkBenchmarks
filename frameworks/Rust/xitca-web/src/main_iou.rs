// used as reference of if/how moving from epoll to io-uring(or mixture of the two) make sense for
// network io.

#![allow(dead_code)]
#![feature(impl_trait_in_assoc_type)]

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod db;
mod ser;
mod util;

use std::{
    cell::RefCell,
    convert::Infallible,
    fmt,
    future::{poll_fn, Future},
    io,
    pin::pin,
};

use futures_util::stream::Stream;
use xitca_http::{
    body::Once,
    date::DateTimeService,
    h1::proto::context::Context,
    http::{
        self,
        const_header_value::{TEXT, TEXT_HTML_UTF8},
        header::{CONTENT_TYPE, SERVER},
        IntoResponse, RequestExt, StatusCode,
    },
    util::service::context::{Context as Ctx, ContextBuilder},
};
use xitca_io::{
    bytes::{Bytes, BytesMut},
    io_uring::IoBuf,
    net::{io_uring::TcpStream as IOUTcpStream, TcpStream},
};
use xitca_service::{fn_service, middleware::UncheckedReady, Service, ServiceExt};

use self::{
    db::Client,
    ser::{json_response, Message},
    util::{QueryParse, DB_URL, SERVER_HEADER_VALUE},
};

fn main() -> io::Result<()> {
    xitca_server::Builder::new()
        .bind("xitca-iou", "0.0.0.0:8080", || {
            Http1IOU::new(
                ContextBuilder::new(|| async {
                    db::create(DB_URL).await.map(|client| State {
                        client,
                        write_buf: RefCell::new(BytesMut::new()),
                    })
                })
                .service(fn_service(handler)),
            )
            .enclosed(UncheckedReady)
        })?
        .build()
        .wait()
}

async fn handler(ctx: Ctx<'_, Request, State>) -> Result<Response, Infallible> {
    let (req, state) = ctx.into_parts();
    let mut res = match req.uri().path() {
        "/plaintext" => {
            let mut res = req.into_response(Bytes::from_static(b"Hello, World!"));
            res.headers_mut().insert(CONTENT_TYPE, TEXT);
            res
        }
        "/json" => json_response(req, &mut state.write_buf.borrow_mut(), &Message::new()).unwrap(),
        "/db" => {
            let world = state.client.get_world().await.unwrap();
            json_response(req, &mut state.write_buf.borrow_mut(), &world).unwrap()
        }
        "/queries" => {
            let num = req.uri().query().parse_query();
            let worlds = state.client.get_worlds(num).await.unwrap();
            json_response(req, &mut state.write_buf.borrow_mut(), worlds.as_slice()).unwrap()
        }
        "/updates" => {
            let num = req.uri().query().parse_query();
            let worlds = state.client.update(num).await.unwrap();
            json_response(req, &mut state.write_buf.borrow_mut(), worlds.as_slice()).unwrap()
        }
        "/fortunes" => {
            use sailfish::TemplateOnce;
            let fortunes = state
                .client
                .tell_fortune()
                .await
                .unwrap()
                .render_once()
                .unwrap();
            let mut res = req.into_response(Bytes::from(fortunes));
            res.headers_mut().append(CONTENT_TYPE, TEXT_HTML_UTF8);
            res
        }
        _ => {
            let mut res = req.into_response(Bytes::new());
            *res.status_mut() = StatusCode::NOT_FOUND;
            res
        }
    };
    res.headers_mut().insert(SERVER, SERVER_HEADER_VALUE);
    Ok(res)
}

struct Http1IOU<S> {
    service: S,
}

impl<S> Http1IOU<S> {
    fn new(service: S) -> Self {
        Self { service }
    }
}

// builder for http service.
impl<S> Service for Http1IOU<S>
where
    S: Service,
{
    type Response = Http1IOUService<S::Response>;
    type Error = S::Error;
    type Future<'f> = impl Future<Output = Result<Self::Response, Self::Error>> + 'f
    where
        Self: 'f,
        (): 'f;

    fn call<'s>(&'s self, _: ()) -> Self::Future<'s>
    where
        (): 's,
    {
        async {
            self.service.call(()).await.map(|service| Http1IOUService {
                service,
                date: DateTimeService::new(),
            })
        }
    }
}

struct Http1IOUService<S> {
    service: S,
    date: DateTimeService,
}

// runner for http service.
impl<S> Service<TcpStream> for Http1IOUService<S>
where
    S: Service<Request, Response = Response>,
    S::Error: fmt::Debug,
{
    type Response = ();
    type Error = io::Error;
    type Future<'f> = impl Future<Output = Result<Self::Response, Self::Error>> + 'f
    where
        Self: 'f,
        TcpStream: 'f;

    fn call<'s>(&'s self, stream: TcpStream) -> Self::Future<'s>
    where
        TcpStream: 's,
    {
        async {
            let mut ctx = Context::<_, 8>::new(self.date.get());
            let mut read_buf = BytesMut::new();
            let mut write_buf = BytesMut::with_capacity(4096);

            let std = stream.into_std()?;
            let stream = IOUTcpStream::from_std(std);

            loop {
                let len = read_buf.len();
                let rem = read_buf.capacity() - len;
                if rem < 4096 {
                    read_buf.reserve(4096 - rem);
                }

                let (res, buf) = stream.read(read_buf.slice(len..)).await;
                read_buf = buf.into_inner();
                if res? == 0 {
                    break;
                }

                while let Some((req, _)) = ctx.decode_head::<{ usize::MAX }>(&mut read_buf).unwrap()
                {
                    let (parts, body) = self.service.call(req).await.unwrap().into_parts();
                    let mut encoder = ctx.encode_head(parts, &body, &mut write_buf).unwrap();
                    let mut body = pin!(body);
                    while let Some(chunk) = poll_fn(|cx| body.as_mut().poll_next(cx)).await {
                        let chunk = chunk.unwrap();
                        encoder.encode(chunk, &mut write_buf);
                    }
                    encoder.encode_eof(&mut write_buf);
                }

                let (res, b) = stream.write_all(write_buf).await;
                write_buf = b;
                write_buf.clear();
                res?;
            }

            stream.shutdown(std::net::Shutdown::Both)
        }
    }
}

type Request = http::Request<RequestExt<()>>;

type Response = http::Response<Once<Bytes>>;

struct State {
    client: Client,
    write_buf: RefCell<BytesMut>,
}
