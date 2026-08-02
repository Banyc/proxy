use common::addr::InternetAddr;
use hyper::{Request, http::uri::Authority};

use super::TunnelError;

const DEFAULT_PORT_HTTP: u16 = 80;
const DEFAULT_PORT_HTTPS: u16 = 443;

pub(crate) fn host_and_port(authority: &Authority) -> &str {
    let authority = authority.as_str();
    match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    }
}

pub(crate) fn redacted_uri(uri: &hyper::Uri) -> String {
    let Some(authority) = uri.authority() else {
        return uri.to_string();
    };
    let host_and_port = host_and_port(authority);
    if host_and_port.len() == authority.as_str().len() {
        return uri.to_string();
    }
    let scheme = match uri.scheme_str() {
        Some(scheme) => format!("{scheme}://"),
        None => String::new(),
    };
    let path_and_query = uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
    format!("{scheme}{host_and_port}{path_and_query}")
}

pub(crate) fn get_authority_from_req<T>(req: &Request<T>) -> Result<InternetAddr, TunnelError> {
    let scheme = req.uri().scheme_str();

    if let Some(authority) = req.uri().authority() {
        return authority_to_internet_addr(authority, scheme);
    }

    let host = req
        .headers()
        .get(hyper::header::HOST)
        .ok_or(TunnelError::HttpNoHost)?
        .to_str()
        .map_err(|_| TunnelError::HttpInvalidHost("non-ascii host header".into()))?;
    let authority = Authority::try_from(host)
        .map_err(|error| TunnelError::HttpInvalidHost(error.to_string()))?;

    authority_to_internet_addr(&authority, scheme)
}

fn authority_has_explicit_port(authority: &Authority) -> bool {
    let authority = host_and_port(authority);
    if authority.starts_with('[') {
        return authority
            .find(']')
            .is_some_and(|closing_bracket| closing_bracket + 1 < authority.len());
    }
    authority.contains(':')
}

fn authority_to_internet_addr(
    authority: &Authority,
    scheme: Option<&str>,
) -> Result<InternetAddr, TunnelError> {
    let host = authority.host();
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let port = match authority.port_u16() {
        Some(port) => port,
        None if authority_has_explicit_port(authority) => {
            return Err(TunnelError::HttpInvalidPort(
                host_and_port(authority).to_owned(),
            ));
        }
        None => match scheme {
            Some("https") => DEFAULT_PORT_HTTPS,
            Some("http") | None => DEFAULT_PORT_HTTP,
            _ => return Err(TunnelError::HttpNoPort),
        },
    };
    Ok(InternetAddr::from_host_and_port(host, port)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::Method;

    #[test]
    fn origin_form_defaults_domain_and_ipv4_to_port_80() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/path")
            .header(hyper::header::HOST, "example.com")
            .body(())
            .unwrap();
        let addr = get_authority_from_req(&req).unwrap();
        assert_eq!(addr.to_string(), "example.com:80");
    }

    #[test]
    fn host_authority_supports_bracketed_ipv6_with_and_without_port() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/path")
            .header(hyper::header::HOST, "[::1]:8080")
            .body(())
            .unwrap();
        let addr = get_authority_from_req(&req).unwrap();
        assert_eq!(addr.to_string(), "[::1]:8080");

        let req2 = Request::builder()
            .method(Method::GET)
            .uri("/path")
            .header(hyper::header::HOST, "[::1]")
            .body(())
            .unwrap();
        let addr2 = get_authority_from_req(&req2).unwrap();
        assert_eq!(addr2.to_string(), "[::1]:80");
    }

    #[test]
    fn absolute_form_authority_takes_precedence_over_host_header() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("http://absolute.example.com:9090/path")
            .header(hyper::header::HOST, "ignored.example.com")
            .body(())
            .unwrap();
        let addr = get_authority_from_req(&req).unwrap();
        assert_eq!(addr.to_string(), "absolute.example.com:9090");
    }

    #[test]
    fn malformed_host_port_is_not_treated_as_missing_port() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header(hyper::header::HOST, "example.com:abc")
            .body(())
            .unwrap();
        let result = get_authority_from_req(&req);
        assert!(result.is_err());
    }

    #[test]
    fn a_password_in_the_request_target_is_not_read_as_a_port() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("http://user:pw@example.com/path")
            .body(())
            .unwrap();
        let addr = get_authority_from_req(&req).unwrap();
        assert_eq!(addr.to_string(), "example.com:80");
    }

    #[test]
    fn a_password_in_the_request_target_never_reaches_the_error() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("http://user:pw@example.com:99999/path")
            .body(())
            .unwrap();
        let e = get_authority_from_req(&req).unwrap_err();
        assert!(matches!(e, TunnelError::HttpInvalidPort(_)), "{e:?}");
        assert!(!format!("{e}").contains("pw"), "{e}");
        assert!(!format!("{e:?}").contains("pw"), "{e:?}");
    }

    #[test]
    fn the_logged_uri_leaves_the_userinfo_out() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("http://user:pw@example.com/path?q=1")
            .body(())
            .unwrap();
        assert_eq!(redacted_uri(req.uri()), "http://example.com/path?q=1");
        assert_eq!(
            host_and_port(req.uri().authority().unwrap()),
            "example.com"
        );
        let req = Request::builder()
            .method(Method::GET)
            .uri("http://example.com:8080/path")
            .body(())
            .unwrap();
        assert_eq!(redacted_uri(req.uri()), "http://example.com:8080/path");
        assert_eq!(
            host_and_port(req.uri().authority().unwrap()),
            "example.com:8080"
        );
    }

    #[test]
    fn host_port_out_of_range_is_rejected() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header(hyper::header::HOST, "example.com:99999")
            .body(())
            .unwrap();
        let result = get_authority_from_req(&req);
        assert!(matches!(result, Err(TunnelError::HttpInvalidPort(_))));
    }
}
