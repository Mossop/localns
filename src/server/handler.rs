use std::{iter::empty, sync::Arc};

use tokio::sync::Mutex;
use trust_dns_server::{
    authority::{Catalog, MessageResponseBuilder},
    client::op::{Header, MessageType, OpCode, ResponseCode},
    resolver::TokioAsyncResolver,
    server::{Request, RequestHandler, ResponseHandler, ResponseInfo},
};

fn serve_failed() -> ResponseInfo {
    let mut header = Header::new();
    header.set_response_code(ResponseCode::ServFail);
    header.into()
}

pub struct Handler {
    pub catalog: Arc<Mutex<Catalog>>,
    pub resolver: TokioAsyncResolver,
}

impl Handler {
    async fn lookup_upstream<R: ResponseHandler>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let request_info = request.request_info();

        log::trace!(
            "Forwarding query for {} to upstream resolver.",
            request_info.query.name()
        );

        let result = match self
            .resolver
            .lookup(
                request_info.query.name(),
                request_info.query.query_type(),
                Default::default(),
            )
            .await
        {
            Ok(lookup) => {
                response_handle
                    .send_response(MessageResponseBuilder::from_message_request(request).build(
                        *request.header(),
                        lookup.record_iter(),
                        empty(),
                        empty(),
                        empty(),
                    ))
                    .await
            }
            Err(e) => {
                log::warn!("Upstream server returned an error: {}", e);
                response_handle
                    .send_response(
                        MessageResponseBuilder::from_message_request(request)
                            .error_msg(request.header(), ResponseCode::Refused),
                    )
                    .await
            }
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
            self.lookup_upstream(request, response_handle).await
        } else {
            catalog.handle_request(request, response_handle).await
        }
    }
}
