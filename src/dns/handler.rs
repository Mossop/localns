use std::sync::Arc;

use hickory_client::op::{Edns, Header, MessageType, OpCode, ResponseCode};
use hickory_server::{
    authority::MessageResponseBuilder,
    server::{Request, RequestHandler, ResponseHandler, ResponseInfo},
};
use tokio::sync::RwLock;
use tracing::instrument;

use crate::dns::ServerState;

use super::server::QueryContext;

fn serve_failed() -> ResponseInfo {
    let mut header = Header::new();
    header.set_response_code(ResponseCode::ServFail);
    header.into()
}

#[derive(Clone)]
pub(crate) struct Handler {
    pub server_state: Arc<RwLock<ServerState>>,
}

impl Handler {
    async fn server_state(&self) -> ServerState {
        let server = self.server_state.read().await;
        server.clone()
    }
}

#[async_trait::async_trait]
impl RequestHandler for Handler {
    #[instrument(fields(
        request.id = request.id(),
        request.protocol = %request.request_info().protocol,
        request.source_address = %request.request_info().src.ip(),
        request.source_port = %request.request_info().src.port(),
        request.qflags = request.header().flags().to_string(),
    ), skip_all)]
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
                tracing::warn!(
                    request_version = req_edns.version(),
                    current_version = our_version,
                    "Invalid request edns version",
                );

                // TODO: should ResponseHandle consume self?
                let result = response_handle
                    .send_response(builder.error_msg(request.header(), ResponseCode::BADVERS))
                    .await;

                // couldn't handle the request
                return match result {
                    Err(e) => {
                        tracing::error!(error = %e, "Request error");
                        serve_failed()
                    }
                    Ok(info) => info,
                };
            }
        }

        let result = match request.message_type() {
            MessageType::Query => match request.op_code() {
                OpCode::Query => {
                    let server_state = self.server_state().await;
                    let mut context = QueryContext::new(server_state.records, server_state.zones);
                    context
                        .perform_query(request.query().original(), request.recursion_desired())
                        .await;

                    response_handle
                        .send_response(builder.build(
                            context.header(request.header()),
                            context.answers(),
                            context.name_servers(),
                            context.soa(),
                            context.additionals(),
                        ))
                        .await
                }
                c => {
                    tracing::warn!(op_code = ?c, "Unimplemented op_code");
                    response_handle
                        .send_response(builder.error_msg(request.header(), ResponseCode::NotImp))
                        .await
                }
            },
            MessageType::Response => {
                tracing::warn!("Got a response as a request from id");
                response_handle
                    .send_response(builder.error_msg(request.header(), ResponseCode::FormErr))
                    .await
            }
        };

        match result {
            Err(e) => {
                tracing::error!(error = %e, "Request failed");
                serve_failed()
            }
            Ok(info) => info,
        }
    }
}
