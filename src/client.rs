use crate::{Error, error::ErrorAndResponse};
use axum_core::body::Body;
use http_body_util::BodyExt;
use hyper::{
    Method, Request, StatusCode,
    client::conn::http1::{self, SendRequest},
};
use hyper_util::rt::TokioIo;
use std::path::PathBuf;
use tokio::{net::UnixStream, task::JoinHandle};

pub struct ClientUnix {
    socket_path: PathBuf,
    sender: SendRequest<Body>,
    join_handle: JoinHandle<Error>,
}

impl ClientUnix {
    pub async fn try_new(socket_path: &str) -> Result<Self, Error> {
        let socket_path = PathBuf::from(socket_path);
        ClientUnix::try_connect(socket_path).await
    }

    pub async fn try_reconnect(self) -> Result<Self, Error> {
        let socket_path = self.socket_path.clone();
        self.abort().await;
        ClientUnix::try_connect(socket_path).await
    }

    pub async fn abort(self) -> Option<Error> {
        self.join_handle.abort();
        self.join_handle.await.ok()
    }

    async fn try_connect(socket_path: PathBuf) -> Result<Self, Error> {
        let stream = TokioIo::new(
            UnixStream::connect(socket_path.clone())
                .await
                .map_err(Error::SocketConnectionInitiation)?,
        );

        let (sender, connection) = http1::handshake(stream).await.map_err(Error::Handhsake)?;

        let join_handle =
            tokio::task::spawn(
                async move { Error::SocketConnectionClosed(connection.await.err()) },
            );

        Ok(ClientUnix {
            socket_path,
            sender,
            join_handle,
        })
    }

    pub async fn send_request(
        &mut self,
        endpoint: &str,
        method: Method,
        headers: &[(&str, &str)],
        body_request: Option<Body>,
    ) -> Result<(StatusCode, Vec<u8>), ErrorAndResponse> {
        let mut request_builder = Request::builder();
        for header in headers {
            request_builder = request_builder.header(header.0, header.1);
        }
        let request = request_builder
            .method(method)
            .uri(format!("http://unix.socket{}", endpoint))
            .body(body_request.unwrap_or(Body::empty()))
            .map_err(|e| ErrorAndResponse::InternalError(Error::RequestBuild(e)))?;

        let response = self
            .sender
            .send_request(request)
            .await
            .map_err(|e| ErrorAndResponse::InternalError(Error::RequestSend(e)))?;

        let status_code = response.status();
        let body_response = response
            .collect()
            .await
            .map_err(|e| ErrorAndResponse::InternalError(Error::ResponseCollect(e)))?
            .to_bytes();

        if !status_code.is_success() {
            return Err(ErrorAndResponse::ResponseUnsuccessful(
                status_code,
                body_response.to_vec(),
            ));
        }
        Ok((status_code, body_response.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{server::Server, util::*};
    use hyper::Method;

    #[tokio::test]
    async fn simple_request() {
        let (_, mut client) = make_client_server("simple_request").await;

        let (status_code, response) = client
            .send_request("/nolanv", Method::GET, &[], None)
            .await
            .expect("client.send_request");

        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(response, "Hello nolanv".as_bytes())
    }

    #[tokio::test]
    async fn simple_404_request() {
        let (_, mut client) = make_client_server("simple_404_request").await;

        let result = client
            .send_request("/nolanv/nope", Method::GET, &[], None)
            .await;

        assert!(matches!(
            result.err(),
            Some(ErrorAndResponse::ResponseUnsuccessful(status_code, _))
                if status_code == StatusCode::NOT_FOUND
        ));
    }

    #[tokio::test]
    async fn multiple_request() {
        let (_, mut client) = make_client_server("multiple_request").await;

        for i in 0..20 {
            let (status_code, response) = client
                .send_request(&format!("/nolanv{}", i), Method::GET, &[], None)
                .await
                .expect("client.send_request");

            assert_eq!(status_code, StatusCode::OK);

            assert_eq!(response, format!("Hello nolanv{}", i).as_bytes())
        }
    }

    #[tokio::test]
    async fn server_not_started() {
        let socket_path = make_socket_path_test("client", "server_not_started");

        let client = ClientUnix::try_new(&socket_path).await;
        assert!(matches!(
            client.err(),
            Some(Error::SocketConnectionInitiation(_))
        ));
    }

    #[tokio::test]
    async fn server_stopped() {
        let (server, mut client) = make_client_server("server_stopped").await;
        server.abort().await;

        let response_result = client.send_request("/nolanv", Method::GET, &[], None).await;
        assert!(matches!(
            response_result.err(),
            Some(ErrorAndResponse::InternalError(Error::RequestSend(e)))
                if e.is_canceled()
        ));

        let _ = Server::try_new(&make_socket_path_test("client", "server_stopped"))
            .await
            .expect("Server::try_new");
        let mut http_client = client.try_reconnect().await.expect("client.try_reconnect");

        let (status_code, response) = http_client
            .send_request("/nolanv", Method::GET, &[], None)
            .await
            .expect("client.send_request");

        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(response, "Hello nolanv".as_bytes())
    }
}
