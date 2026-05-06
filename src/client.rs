use anyhow::Result;

use crate::protocol::{Request, Response};

#[derive(Debug, Default)]
pub struct Client;

impl Client {
    pub fn new() -> Self {
        Self
    }

    pub async fn send(&self, request: Request) -> Result<Response> {
        Ok(Response::NotImplemented {
            command: request.name().to_owned(),
        })
    }
}
