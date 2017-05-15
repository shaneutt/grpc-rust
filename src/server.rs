use std::sync::Arc;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::panic::catch_unwind;
use std::panic::AssertUnwindSafe;

use futures_cpupool::CpuPool;

use bytes::Bytes;

use httpbis::HttpError;
use httpbis::Header;
use httpbis::Headers;
use httpbis::server::HttpServer;
use httpbis::server::ServerTlsOption;

use futures::Future;
use futures::stream;
use futures::stream::Stream;

use method::*;
use error::*;
use httpbis::futures_misc::*;
use grpc::*;
use grpc_frame::*;
use httpbis::http_common::*;
use httpbis::server_conf::*;
use httpbis::misc::any_to_string;
use req::*;
use resp::*;
use metadata::Metadata;


pub trait MethodHandler<Req, Resp>
    where
        Req : Send + 'static,
        Resp : Send + 'static,
{
    fn handle(&self, m: RequestOptions, req: StreamingRequest<Req>) -> StreamingResponse<Resp>;
}

pub struct MethodHandlerUnary<F> {
    f: Arc<F>
}

pub struct MethodHandlerServerStreaming<F> {
    f: Arc<F>
}

pub struct MethodHandlerClientStreaming<F> {
    f: Arc<F>
}

pub struct MethodHandlerBidi<F> {
    f: Arc<F>
}

impl<F> GrpcStreamingFlavor for MethodHandlerUnary<F> {
    type Flavor = GrpcStreamingUnary;

    fn streaming() -> GrpcStreaming {
        GrpcStreaming::Unary
    }
}

impl<F> GrpcStreamingFlavor for MethodHandlerClientStreaming<F> {
    type Flavor = GrpcStreamingClientStreaming;

    fn streaming() -> GrpcStreaming {
        GrpcStreaming::ClientStreaming
    }
}

impl<F> GrpcStreamingFlavor for MethodHandlerServerStreaming<F> {
    type Flavor = GrpcStreamingServerStreaming;

    fn streaming() -> GrpcStreaming {
        GrpcStreaming::ServerStreaming
    }
}

impl<F> GrpcStreamingFlavor for MethodHandlerBidi<F> {
    type Flavor = GrpcStreamingBidi;

    fn streaming() -> GrpcStreaming {
        GrpcStreaming::Bidi
    }
}


impl<F> MethodHandlerUnary<F> {
    pub fn new<Req, Resp>(f: F) -> Self
        where
            Req : Send + 'static,
            Resp : Send + 'static,
            F : Fn(RequestOptions, Req) -> SingleResponse<Resp> + Send + 'static,
    {
        MethodHandlerUnary {
            f: Arc::new(f),
        }
    }
}

impl<F> MethodHandlerClientStreaming<F> {
    pub fn new<Req, Resp>(f: F) -> Self
        where
            Req : Send + 'static,
            Resp : Send + 'static,
            F : Fn(RequestOptions, StreamingRequest<Req>) -> SingleResponse<Resp> + Send + 'static,
    {
        MethodHandlerClientStreaming {
            f: Arc::new(f),
        }
    }
}

impl<F> MethodHandlerServerStreaming<F> {
    pub fn new<Req, Resp>(f: F) -> Self
        where
            Req : Send + 'static,
            Resp : Send + 'static,
            F : Fn(RequestOptions, Req) -> StreamingResponse<Resp> + Send + 'static,
    {
        MethodHandlerServerStreaming {
            f: Arc::new(f),
        }
    }
}

impl<F> MethodHandlerBidi<F> {
    pub fn new<Req, Resp>(f: F) -> Self
        where
            Req : Send + 'static,
            Resp : Send + 'static,
            F : Fn(RequestOptions, StreamingRequest<Req>) -> StreamingResponse<Resp> + Send + 'static,
    {
        MethodHandlerBidi {
            f: Arc::new(f),
        }
    }
}

impl<Req, Resp, F> MethodHandler<Req, Resp> for MethodHandlerUnary<F>
    where
        Req : Send + 'static,
        Resp : Send + 'static,
        F : Fn(RequestOptions, Req) -> SingleResponse<Resp> + Send + Sync + 'static,
{
    fn handle(&self, m: RequestOptions, req: StreamingRequest<Req>) -> StreamingResponse<Resp> {
        let f = self.f.clone();
        SingleResponse::new(
            stream_single(req.0).and_then(move |req| f(m, req).0))
                .into_stream()
    }
}

impl<Req : Send + 'static, Resp : Send + 'static, F> MethodHandler<Req, Resp> for MethodHandlerClientStreaming<F>
    where
        Resp : Send + 'static,
        F : Fn(RequestOptions, StreamingRequest<Req>) -> SingleResponse<Resp> + Send + Sync + 'static,
{
    fn handle(&self, m: RequestOptions, req: StreamingRequest<Req>) -> StreamingResponse<Resp> {
        ((self.f)(m, req)).into_stream()
    }
}

impl<Req, Resp, F> MethodHandler<Req, Resp> for MethodHandlerServerStreaming<F>
    where
        Req : Send + 'static,
        Resp : Send + 'static,
        F : Fn(RequestOptions, Req) -> StreamingResponse<Resp> + Send + Sync + 'static,
{
    fn handle(&self, o: RequestOptions, req: StreamingRequest<Req>) -> StreamingResponse<Resp> {
        let f = self.f.clone();
        StreamingResponse(Box::new(
            stream_single(req.0)
                .and_then(move |req| f(o, req).0)))
    }
}

impl<Req, Resp, F> MethodHandler<Req, Resp> for MethodHandlerBidi<F>
    where
        Req : Send + 'static,
        Resp : Send + 'static,
        F : Fn(RequestOptions, StreamingRequest<Req>) -> StreamingResponse<Resp> + Send + Sync + 'static,
{
    fn handle(&self, m: RequestOptions, req: StreamingRequest<Req>) -> StreamingResponse<Resp> {
        (self.f)(m, req)
    }
}


trait MethodHandlerDispatch {
    fn start_request(&self, m: RequestOptions, grpc_frames: StreamingRequest<Vec<u8>>)
                     -> StreamingResponse<Vec<u8>>;
}

struct MethodHandlerDispatchImpl<Req, Resp> {
    desc: Arc<MethodDescriptor<Req, Resp>>,
    method_handler: Box<MethodHandler<Req, Resp> + Sync + Send>,
}

impl<Req, Resp> MethodHandlerDispatch for MethodHandlerDispatchImpl<Req, Resp>
    where
        Req : Send + 'static,
        Resp : Send + 'static,
{
    fn start_request(&self, o: RequestOptions, req_grpc_frames: StreamingRequest<Vec<u8>>)
                     -> StreamingResponse<Vec<u8>>
    {
        let desc = self.desc.clone();
        let req = req_grpc_frames.0.and_then(move |frame| desc.req_marshaller.read(&frame));
        let resp =
            catch_unwind(AssertUnwindSafe(|| self.method_handler.handle(o, StreamingRequest::new(req))));
        match resp {
            Ok(resp) => {
                let desc_copy = self.desc.clone();
                resp.and_then_items(move |resp| {
                    desc_copy.resp_marshaller.write(&resp)
                })
            }
            Err(e) => {
                let message = any_to_string(e);
                StreamingResponse::err(Error::Panic(message))
            }
        }
    }
}

pub struct ServerMethod {
    name: String,
    dispatch: Box<MethodHandlerDispatch + Sync + Send>,
}

impl ServerMethod {
    pub fn new<Req, Resp, H>(method: Arc<MethodDescriptor<Req, Resp>>, handler: H) -> ServerMethod
        where
            Req : Send + 'static,
            Resp : Send + 'static,
            H : MethodHandler<Req, Resp> + 'static + Sync + Send,
    {
        ServerMethod {
            name: method.name.clone(),
            dispatch: Box::new(MethodHandlerDispatchImpl {
                desc: method,
                method_handler: Box::new(handler),
            }),
        }
    }
}

pub struct ServerServiceDefinition {
    methods: Vec<ServerMethod>,
}

impl ServerServiceDefinition {
    pub fn new(methods: Vec<ServerMethod>) -> ServerServiceDefinition {
        ServerServiceDefinition {
            methods: methods,
        }
    }

    /// Join multiple service definitions into one
    pub fn join<I>(iter: I) -> ServerServiceDefinition
        where I : IntoIterator<Item=ServerServiceDefinition>
    {
        ServerServiceDefinition {
            methods: iter.into_iter().flat_map(|s| s.methods).collect()
        }
    }

    pub fn find_method(&self, name: &str) -> &ServerMethod {
        self.methods.iter()
            .filter(|m| m.name == name)
            .next()
            .expect(&format!("unknown method: {}", name))
    }

    pub fn handle_method(&self, name: &str, o: RequestOptions, message: StreamingRequest<Vec<u8>>)
        -> StreamingResponse<Vec<u8>>
    {
        self.find_method(name).dispatch.start_request(o, message)
    }
}

#[derive(Default, Debug, Clone)]
pub struct ServerConf {
    pub http: HttpServerConf,
}


pub struct Server {
    server: HttpServer,
}

impl Server {
    /// Without TLS
    pub fn new_plain<A : ToSocketAddrs>(
        addr: A,
        conf: ServerConf,
        service_definition: ServerServiceDefinition)
            -> Server
    {
        Server::new(addr, ServerTlsOption::Plain, conf, service_definition)
    }

    /// Without TLS and execute handler in given CpuPool
    pub fn new_plain_pool<A : ToSocketAddrs>(
        addr: A,
        conf: ServerConf,
        service_definition: ServerServiceDefinition,
        cpu_pool: CpuPool)
            -> Server
    {
        Server::new_pool(addr, ServerTlsOption::Plain, conf, service_definition, cpu_pool)
    }

    pub fn new<A : ToSocketAddrs>(
        addr: A,
        tls: ServerTlsOption,
        conf: ServerConf,
        service_definition: ServerServiceDefinition)
            -> Server
    {
        Server::with_starter(addr, tls, conf, service_definition, CallStarterSync)
    }

    pub fn new_pool<A : ToSocketAddrs>(
        addr: A,
        tls: ServerTlsOption,
        conf: ServerConf,
        service_definition: ServerServiceDefinition,
        cpu_pool: CpuPool)
            -> Server
    {
        Server::with_starter(addr, tls, conf, service_definition, CallStarterCpupool {
            cpu_pool: cpu_pool,
        })
    }

    fn with_starter<A : ToSocketAddrs, S : CallStarter>(
        addr: A,
        tls: ServerTlsOption,
        conf: ServerConf,
        service_definition: ServerServiceDefinition,
        call_starter: S)
            -> Server
    {
        let mut conf = conf;
        conf.http.thread_name =
            Some(conf.http.thread_name.unwrap_or_else(|| "grpc-server-loop".to_owned()));

        let service_definition = Arc::new(service_definition);
        Server {
            server: HttpServer::new(addr, tls, conf.http, GrpcHttpService {
                service_definition: service_definition.clone(),
                call_starter: call_starter,
            })
        }
    }

    pub fn local_addr(&self) -> &SocketAddr {
        self.server.local_addr()
    }

    pub fn is_alive(&self) -> bool {
        self.server.is_alive()
    }
}

/// Utility to start a call
trait CallStarter : Send + 'static {
    fn start(
        &self,
        service_definition: &Arc<ServerServiceDefinition>,
        name: &str,
        o: RequestOptions,
        message: StreamingRequest<Vec<u8>>)
        -> StreamingResponse<Vec<u8>>;
}

/// Start a call in current task
struct CallStarterSync;

impl CallStarter for CallStarterSync {
    fn start(
        &self,
        service_definition: &Arc<ServerServiceDefinition>,
        name: &str,
        o: RequestOptions,
        message: StreamingRequest<Vec<u8>>)
        -> StreamingResponse<Vec<u8>>
    {
        service_definition.handle_method(name, o, message)
    }
}

/// Start a call in cpupool
struct CallStarterCpupool {
    cpu_pool: CpuPool,
}

impl CallStarter for CallStarterCpupool {
    fn start(
        &self,
        service_definition: &Arc<ServerServiceDefinition>,
        name: &str,
        o: RequestOptions,
        message: StreamingRequest<Vec<u8>>)
        -> StreamingResponse<Vec<u8>>
    {
        let service_definition = service_definition.clone();
        let name = name.to_owned();
        let f = self.cpu_pool.spawn_fn(move || {
            service_definition.handle_method(&name, o, message).0
        });
        StreamingResponse::new(f)
    }
}

/// Implementation of gRPC over http2 HttpService
struct GrpcHttpService<S : CallStarter> {
    service_definition: Arc<ServerServiceDefinition>,
    call_starter: S,
}


/// Create HTTP response for gRPC error
fn http_response_500(message: &str) -> HttpResponse {
    // TODO: HttpResponse::headers
    let headers = Headers(vec![
        Header::new(":status", "500"),
        Header::new(HEADER_GRPC_MESSAGE, message.to_owned()),
    ]);
    HttpResponse::headers_and_stream(headers, HttpPartStream::empty())
}

impl<S : CallStarter> HttpService for GrpcHttpService<S> {
    fn start_request(&self, headers: Headers, req: HttpPartStream) -> HttpResponse {

        let path = match headers.get_opt(":path") {
            Some(path) => path.to_owned(),
            None => return http_response_500("no :path header"),
        };

        let grpc_request = GrpcFrameFromHttpFramesStreamRequest::new(req);

        let metadata = match Metadata::from_headers(headers) {
            Ok(metadata) => metadata,
            Err(_) => return http_response_500("decode metadata error"),
        };

        // TODO: catch unwind
        let grpc_response = self.call_starter.start(
            &self.service_definition,
            &path,
            RequestOptions { metadata: metadata },
            StreamingRequest::new(grpc_request));

        HttpResponse::new(grpc_response.0.map_err(HttpError::from).map(|(metadata, grpc_frames)| {
            let mut init_headers = Headers(vec![
                Header::new(":status", "200"),
                Header::new("content-type", "application/grpc"),
            ]);

            init_headers.extend(metadata.into_headers());

            let s2 = grpc_frames
                .drop_metadata() // TODO
                .map(|frame| HttpStreamPart::intermediate_data(Bytes::from(write_grpc_frame_to_vec(&frame))))
                .then(|result| {
                    match result {
                        Ok(part) => {
                            let r: Result<_, HttpError> = Ok(part);
                            r
                        }
                        Err(e) =>
                            Ok(HttpStreamPart::last_headers(
                                match e {
                                    Error::GrpcMessage(GrpcMessageError { grpc_status, grpc_message }) => {
                                        Headers(vec![
                                            Header::new(":status", "500"),
                                            // TODO: check nonzero
                                            Header::new(HEADER_GRPC_STATUS, format!("{}", grpc_status)),
                                            // TODO: escape invalid
                                            Header::new(HEADER_GRPC_MESSAGE, grpc_message),
                                        ])
                                    }
                                    e => {
                                        Headers(vec![
                                            Header::new(":status", "500"),
                                            Header::new(HEADER_GRPC_MESSAGE, format!("error: {:?}", e)),
                                        ])
                                    }
                                }
                            ))
                    }
                })
                .map_err(HttpError::from);

            let s3 = stream::once(Ok(HttpStreamPart::last_headers(Headers(vec![
                Header::new(HEADER_GRPC_STATUS, "0"),
            ]))));

            let http_parts = HttpPartStream::new(s2.chain(s3));

            (init_headers, http_parts)
        }))
    }
}
