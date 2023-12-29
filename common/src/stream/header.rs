use crate::header::route::RouteRequest;

use super::StreamAddr;

pub type StreamRequestHeader<ST> = RouteRequest<StreamAddr<ST>>;
