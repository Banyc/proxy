use std::{collections::HashMap, io, num::NonZeroU8, sync::Arc};

use common::stream::{HasIoAddr, OwnedIoStream};

use crate::socks5::messages::{
    MethodIdentifier, NegotiationRequest, NegotiationResponse,
    sub_negotiations::{UsernamePasswordRequest, UsernamePasswordResponse, UsernamePasswordStatus},
};

const AUTH_FAILURE: NonZeroU8 = NonZeroU8::MIN;

#[derive(Debug, Clone, Default)]
pub(crate) struct Users {
    users: HashMap<Arc<[u8]>, Arc<[u8]>>,
}
impl Users {
    pub(crate) fn new(users: HashMap<Arc<[u8]>, Arc<[u8]>>) -> Self {
        Self { users }
    }

    pub(crate) async fn negotiate<Stream>(&self, mut stream: Stream) -> io::Result<Stream>
    where
        Stream: OwnedIoStream + HasIoAddr + std::fmt::Debug,
    {
        let negotiation_request = NegotiationRequest::decode(&mut stream).await?;

        // Username/password authentication
        if !self.users.is_empty()
            && negotiation_request
                .methods
                .contains(&MethodIdentifier::UsernamePassword)
        {
            let negotiation_response = NegotiationResponse {
                method: Some(MethodIdentifier::UsernamePassword),
            };
            negotiation_response.encode(&mut stream).await?;

            let stream = self.username_password(stream).await?;
            return Ok(stream);
        }

        // No authentication
        let allow_no_auth = self.users.is_empty();
        if !allow_no_auth
            || !negotiation_request
                .methods
                .contains(&MethodIdentifier::NoAuth)
        {
            let negotiation_response = NegotiationResponse { method: None };
            negotiation_response.encode(&mut stream).await?;
            return Err(io::Error::other("No auth method supported"));
        }
        let negotiation_response = NegotiationResponse {
            method: Some(MethodIdentifier::NoAuth),
        };
        negotiation_response.encode(&mut stream).await?;

        Ok(stream)
    }

    async fn username_password<Stream>(&self, mut stream: Stream) -> io::Result<Stream>
    where
        Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let request = UsernamePasswordRequest::decode(&mut stream).await?;
        if let Err(e) = self.authenticate(&request) {
            let response = UsernamePasswordResponse {
                status: UsernamePasswordStatus::Failure(AUTH_FAILURE),
            };
            response.encode(&mut stream).await?;
            return Err(e);
        }
        let response = UsernamePasswordResponse {
            status: UsernamePasswordStatus::Success,
        };
        response.encode(&mut stream).await?;
        Ok(stream)
    }

    fn authenticate(&self, request: &UsernamePasswordRequest) -> io::Result<()> {
        let expected = self.users.get(request.username());
        let filler = vec![0; request.password().len()];
        let matches = Self::password_matches(
            request.password(),
            expected.map_or(filler.as_slice(), |password| password),
        );
        if expected.is_none() {
            return Err(io::Error::other(format!(
                "Username not found: {}",
                String::from_utf8_lossy(request.username())
            )));
        }
        if !matches {
            return Err(Self::password_incorrect_error(request));
        }
        Ok(())
    }

    fn password_matches(offered: &[u8], expected: &[u8]) -> bool {
        constant_time_eq::constant_time_eq(offered, expected)
    }

    fn password_incorrect_error(request: &UsernamePasswordRequest) -> io::Error {
        io::Error::other(format!(
            "Password incorrect: {{ username: {} }}",
            String::from_utf8_lossy(request.username())
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socks5::messages::sub_negotiations::USERNAME_PASSWORD_VERSION;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn users(list: &[(&[u8], &[u8])]) -> Users {
        Users::new(
            list.iter()
                .map(|(u, p)| ((*u).into(), (*p).into()))
                .collect(),
        )
    }

    async fn login(users: &Users, username: &[u8], password: &[u8]) -> Vec<u8> {
        let (server, mut client) = tokio::io::duplex(1024);
        let request = UsernamePasswordRequest::new(username, password).unwrap();
        request.encode(&mut client).await.unwrap();
        let _ = users.username_password(server).await;
        client.shutdown().await.unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        reply
    }

    #[tokio::test]
    async fn a_rejected_login_never_says_whether_the_username_exists() {
        let users = users(&[(b"alice", b"hunter2")]);
        let no_such_user = login(&users, b"bob", b"hunter2").await;
        let wrong_password = login(&users, b"alice", b"hunter3").await;
        assert_eq!(
            no_such_user, wrong_password,
            "the reply tells the two failures apart"
        );
        let ok = login(&users, b"alice", b"hunter2").await;
        assert_ne!(ok, no_such_user);
        assert_eq!(
            ok,
            vec![
                USERNAME_PASSWORD_VERSION,
                UsernamePasswordStatus::Success.into()
            ]
        );
    }

    #[test]
    fn only_the_exact_password_matches() {
        let matches = Users::password_matches;
        assert!(matches(b"hunter2", b"hunter2"));
        assert!(!matches(b"hunter3", b"hunter2"));
        assert!(!matches(b"Hunter2", b"hunter2"));
        assert!(!matches(b"hunter", b"hunter2"));
        assert!(!matches(b"hunter22", b"hunter2"));
        assert!(!matches(b"", b"hunter2"));
        assert!(matches(b"", b""));
    }

    #[test]
    fn a_rejected_login_never_logs_the_password() {
        let request = UsernamePasswordRequest::new(b"alice", b"hunter2").unwrap();
        let err = Users::password_incorrect_error(&request);
        for rendered in [format!("{err}"), format!("{err:?}")] {
            assert!(
                !rendered.contains("hunter2"),
                "the password reached the log: {rendered}"
            );
            assert!(
                rendered.contains("alice"),
                "the username is what makes the log useful: {rendered}"
            );
        }
    }
}
