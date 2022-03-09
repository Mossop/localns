use std::sync::Arc;

use tokio::sync::Mutex;
use trust_dns_server::{
    authority::{Catalog, MessageResponseBuilder},
    client::{
        op::{Header, MessageType, OpCode, ResponseCode},
        rr::Record,
    },
    server::{Request, RequestHandler, ResponseHandler, ResponseInfo},
};

use crate::record::RData;
use crate::upstream::Upstream;

fn serve_failed() -> ResponseInfo {
    let mut header = Header::new();
    header.set_response_code(ResponseCode::ServFail);
    header.into()
}

pub struct Handler {
    pub catalog: Arc<Mutex<Catalog>>,
    pub upstream: Option<Upstream>,
}

impl Handler {
    async fn upstream<R: ResponseHandler>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let request_info = request.request_info();

        let result = if let Some(ref upstream) = self.upstream {
            match upstream.lookup(request_info.query.original().clone()).await {
                Some(response) => {
                    let soa = response.soa.map(|ref s| {
                        Record::from_rdata(
                            request_info.query.name().into(),
                            300,
                            RData::SOA(s.clone()),
                        )
                    });

                    let mut response_header = Header::response_from_request(request.header());
                    response_header.set_authoritative(false);
                    response_header.set_recursion_available(false);

                    response_handle
                        .send_response(MessageResponseBuilder::from_message_request(request).build(
                            response_header,
                            &response.answers,
                            &response.name_servers,
                            &soa,
                            &response.name_servers,
                        ))
                        .await
                }
                None => {
                    response_handle
                        .send_response(
                            MessageResponseBuilder::from_message_request(request)
                                .error_msg(request.header(), ResponseCode::Refused),
                        )
                        .await
                }
            }
        } else {
            response_handle
                .send_response(
                    MessageResponseBuilder::from_message_request(request)
                        .error_msg(request.header(), ResponseCode::Refused),
                )
                .await
        };

        return match result {
            Err(e) => {
                log::error!("failed to send response: {}", e);
                serve_failed()
            }
            Ok(r) => r,
        };
    }
}

#[async_trait::async_trait]
impl RequestHandler for Handler {
    async fn handle_request<R: ResponseHandler>(
        &self,
        request: &Request,
        response_handle: R,
    ) -> ResponseInfo {
        let catalog = self.catalog.lock().await;

        if request.message_type() == MessageType::Query
            && request.op_code() == OpCode::Query
            && catalog.find(request.request_info().query.name()).is_none()
        {
            self.upstream(request, response_handle).await
        } else {
            catalog.handle_request(request, response_handle).await
        }
    }
}
