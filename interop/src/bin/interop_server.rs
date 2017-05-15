extern crate protobuf;
extern crate grpc;
extern crate futures;
extern crate futures_cpupool;
extern crate env_logger;

extern crate grpc_interop;
use grpc_interop::*;

use std::thread;

use futures::stream::Stream;
use futures::stream;
use futures::Future;

use grpc::futures_grpc::*;
use grpc::error::*;

static DICTIONARY: &'static str = "ABCDEFGHIJKLMNOPQRSTUVabcdefghijklmnoqprstuvwxyz0123456789";
// Note: due to const restrictions, this is calculated by hand.
static DICTIONARY_SIZE: usize = 58;

/**
 * Returns a Vec<u8> with a given size containing printable u8s.
 */
fn make_string(size: usize) -> Vec<u8> {
    let dict = DICTIONARY.to_owned().into_bytes();
    let mut result = Vec::<u8>::with_capacity(size);

    for n in 0..size {
        result.push(dict[n%DICTIONARY_SIZE]);
    }

    return result;
}

struct TestServerImpl {}

impl TestService for TestServerImpl {
    fn empty_call(&self, _o: grpc::RequestOptions, _: Empty) -> grpc::SingleResponse<Empty> {
        grpc::SingleResponse::completed(Empty::new())
    }

    fn unary_call(&self, _o: grpc::RequestOptions, mut req: SimpleRequest) -> grpc::SingleResponse<SimpleResponse> {
        if req.get_response_status().get_code() != 0 {
            return grpc::SingleResponse::no_metadata(futures::failed(grpc::Error::GrpcMessage(GrpcMessageError {
                grpc_status: req.get_response_status().get_code(),
                grpc_message: req.mut_response_status().take_message(),
            })));
        }

        let mut payload = Payload::new();
        payload.set_body(make_string(req.get_response_size() as usize));
        let mut response = SimpleResponse::new();
        response.set_payload(payload);
        grpc::SingleResponse::completed(response)
    }

    // TODO: is this needed? I can't find it implemented in grpc-go/interop/client/client.go
    fn cacheable_unary_call(&self, _o: grpc::RequestOptions, _: SimpleRequest) -> grpc::SingleResponse<SimpleResponse> {
        // TODO: implement fully
        grpc::SingleResponse::completed(SimpleResponse::new())
    }

    fn streaming_output_call(&self, _o: grpc::RequestOptions, mut req: StreamingOutputCallRequest) -> grpc::StreamingResponse<StreamingOutputCallResponse> {
        let sizes = req.take_response_parameters().into_iter().map(|res| Ok(res.get_size() as usize));
        let output = stream::iter(sizes).map(|size| {
            let mut response = StreamingOutputCallResponse::new();
            let mut payload = Payload::new();
            payload.set_body(make_string(size));
            response.set_payload(payload);
            response
        });
        grpc::StreamingResponse::no_metadata(output)
    }

    fn streaming_input_call(&self, _o: grpc::RequestOptions, req_stream: grpc::StreamingRequest<StreamingInputCallRequest>) -> grpc::SingleResponse<StreamingInputCallResponse> {
        let return_stream = req_stream.0
            .map(|req| req.get_payload().body.len() as i32)
            .fold(0, |a, b| futures::finished::<_, grpc::Error>(a + b))
            .map(|aggregate_size| {
                let mut response = StreamingInputCallResponse::new();
                response.set_aggregated_payload_size(aggregate_size);
                response
            });
        grpc::SingleResponse::no_metadata(return_stream)
    }

    fn full_duplex_call(&self, _o: grpc::RequestOptions, req_stream: grpc::StreamingRequest<StreamingOutputCallRequest>)
        -> grpc::StreamingResponse<StreamingOutputCallResponse>
    {
        let response = req_stream.0.map(|mut req| {
            if req.get_response_status().get_code() != 0 {
                let s: GrpcStreamSend<StreamingOutputCallResponse> = Box::new(stream::once(Err(grpc::Error::GrpcMessage(GrpcMessageError {
                    grpc_status: req.get_response_status().get_code(),
                    grpc_message: req.mut_response_status().take_message(),
                }))));
                return s;
            }

            let sizes = req.take_response_parameters().into_iter().map(|res| Ok(res.get_size() as usize));
            let ss: GrpcStreamSend<StreamingOutputCallResponse> = Box::new(stream::iter(sizes).map(|size| {
                let mut response = StreamingOutputCallResponse::new();
                let mut payload = Payload::new();
                payload.set_body(make_string(size));
                response.set_payload(payload);
                response
            }));
            ss
        }).flatten();
        grpc::StreamingResponse::no_metadata(response)
    }

    // TODO: implement this if we find an interop client that needs it.
    fn half_duplex_call(&self, _o: grpc::RequestOptions, _: grpc::StreamingRequest<StreamingOutputCallRequest>)
        -> grpc::StreamingResponse<StreamingOutputCallResponse>
    {
        grpc::StreamingResponse::empty()
    }
}

fn main() {
    env_logger::init().expect("env_logger::init");

    let _server = TestServiceServer::new(("::", DEFAULT_PORT), Default::default(), TestServerImpl {});

    loop {
        thread::park();
    }
}
