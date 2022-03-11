use std::sync::Arc;

use tokio::sync::Mutex;
use trust_dns_server::{
    authority::MessageResponseBuilder,
    client::op::{Edns, Header, MessageType, OpCode, ResponseCode},
    server::{Request, RequestHandler, ResponseHandler, ResponseInfo},
};

use super::server::Server;

fn serve_failed() -> ResponseInfo {
    let mut header = Header::new();
    header.set_response_code(ResponseCode::ServFail);
    header.into()
}

pub(super) struct Handler {
    pub server: Arc<Mutex<Server>>,
}

impl Handler {
    async fn server(&self) -> Server {
        let server = self.server.lock().await;
        server.clone()
    }
}

#[async_trait::async_trait]
impl RequestHandler for Handler {
    async fn handle_request<R: ResponseHandler>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let mut builder = MessageResponseBuilder::from_message_request(request);

        // check if it's edns
        if let Some(req_edns) = request.edns() {
            let mut resp_edns: Edns = Edns::new();

            // check our version against the request
            // TODO: what version are we?
            let our_version = 0;
            resp_edns.set_dnssec_ok(false);
            resp_edns.set_max_payload(req_edns.max_payload().max(512));
            resp_edns.set_version(our_version);
            builder.edns(resp_edns);

            if req_edns.version() > our_version {
                log::warn!(
                    "Request edns version greater than {}: {}",
                    our_version,
                    req_edns.version()
                );

                // TODO: should ResponseHandle consume self?
                let result = response_handle
                    .send_response(builder.error_msg(request.header(), ResponseCode::BADVERS))
                    .await;

                // couldn't handle the request
                return match result {
                    Err(e) => {
                        log::error!("request error: {}", e);
                        serve_failed()
                    }
                    Ok(info) => info,
                };
            }
        }

        let result = match request.message_type() {
            MessageType::Query => match request.op_code() {
                OpCode::Query => {
                    let server = self.server().await;
                    let query = request.request_info().query;
                    let query_result = server.query(query, request.recursion_desired()).await;
                    response_handle
                        .send_response(builder.build(
                            query_result.header(request.header()),
                            query_result.answers(),
                            query_result.name_servers(),
                            query_result.soa(),
                            query_result.additionals(),
                        ))
                        .await
                }
                c => {
                    log::warn!("Unimplemented op_code: {:?}", c);
                    response_handle
                        .send_response(builder.error_msg(request.header(), ResponseCode::NotImp))
                        .await
                }
            },
            MessageType::Response => {
                log::warn!("got a response as a request from id: {}", request.id());
                response_handle
                    .send_response(builder.error_msg(request.header(), ResponseCode::FormErr))
                    .await
            }
        };

        match result {
            Err(e) => {
                log::error!("Request failed: {}", e);
                serve_failed()
            }
            Ok(info) => info,
        }
    }
}
