use std::sync::Arc;

use tokio::sync::Mutex;
use trust_dns_server::{
    authority::{Catalog, MessageResponseBuilder},
    client::{
        op::{Edns, Header, MessageType, OpCode, ResponseCode},
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
        let mut builder = MessageResponseBuilder::from_message_request(request);

        // check if it's edns
        if let Some(req_edns) = request.edns() {
            let mut response_header = Header::response_from_request(request.header());

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
                    "request edns version greater than {}: {}",
                    our_version,
                    req_edns.version()
                );
                response_header.set_response_code(ResponseCode::BADVERS);

                // TODO: should ResponseHandle consume self?
                let result = response_handle
                    .send_response(builder.build_no_records(response_header))
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
                    response_header.set_authoritative(response.authoritative);
                    response_header.set_recursion_available(response.recursion_available);

                    response_handle
                        .send_response(builder.build(
                            response_header,
                            &response.answers,
                            &response.name_servers,
                            &soa,
                            &response.additionals,
                        ))
                        .await
                }
                None => {
                    response_handle
                        .send_response(builder.error_msg(request.header(), ResponseCode::Refused))
                        .await
                }
            }
        } else {
            response_handle
                .send_response(builder.error_msg(request.header(), ResponseCode::Refused))
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
