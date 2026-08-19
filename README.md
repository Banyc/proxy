# Proxy

## Transport boundary

Proxy may select TCP or RTP as the transport beneath mux, but transport-independent proxy and mux behavior should depend only on their common reliable-stream contract. Keep RTP-specific setup and exceptional behavior in the RTP-facing adapter so switching the transport does not require another mux state machine.

## Architecture

<div style="background-color: white">
  
  ![arch](img/arch.drawio.png)
</div>

- Access Server:
  - as an access point, it:
    - implements TCP/UDP listeners, so that it can direct traffic from a process that does not understand the proxy protocol to the proxy
    - pre-defines the proxy chain, so that the downstream process does not need to know the proxy chain
- Proxy Client:
  - as a start of the proxy chain, it:
    - implements the proxy protocol, so that it can direct traffic to the proxy
    - defines the proxy chain, so that the client user can dynamically change the proxy chain without re-configuring those proxies
- Proxy Server:
  - as an traffic proxy, it:
    - is only responsible to a fragment of the stream (TCP) or a datagram (UDP), so that those responsibilities can be stacked and form a proxy chain

## Named reverse tunnels

A private-side initiator can establish an outbound tunnel to a public-side responder and register it under a name. Neither endpoint has a destination: the destination still comes from the normal proxy-chain request header. The public side can use the registered tunnel as either a stream or UDP hop; one mux substream carries each UDP flow and preserves its datagram boundaries.

- The same `revtuntcp://name` and `revtunrtp://name` addresses work in stream and UDP chains. The initiator and responder use the same `header_key` to authenticate tunnel registration. A chain entry for the virtual hop uses that key for its normal proxy request header as well. See `server/config.toml` for configuration examples.
- Every hop in a stream or UDP proxy chain may define its own `payload_key`, while preserving the existing payload-key termination/layering bullet.
- Optional payload encryption terminates at the public proxy-chain entry and the private initiator, which must share the exact same `payload_key`. The virtual chain entry (`revtuntcp://name` / `revtunrtp://name`) carries that key as its payload key; the responder is transport-only and intentionally accepts no `payload_key`.
- The access client nests those payload layers in chain order, and each matching proxy server removes exactly its own layer before forwarding traffic to the next hop. A hop's upstream entry and its proxy server must use the same key.

## Mux proxy UDP flows

The `tcpmux` and `rtpmux` proxy servers accept both stream and UDP flows over a single mux connection. Every mux substream starts with a flow-kind byte, and a UDP flow is framed exactly like a reverse-tunnel UDP flow, so the wire format on a stream is the same in both cases:

```
0: [kind=0 | proxy protocol]                          # stream flow
1: [kind=1 | u16 datagram length | payload]...        # UDP flow (udp_mux framing)
```

A `tcpmux://host:port` / `rtpmux://host:port` address therefore works as a hop in a UDP proxy chain: the client dials the mux proxy, opens a UDP-flow substream, and each datagram carries the same routed/compact request framing as any other UDP hop. The proxy server dispatches the flow to its UDP proxy handler, which shares the listener's `header_key`/`payload_key`.

RTP-mux owns a fixed, lane-aware FEC policy: the interactive lane enables FEC by default (with optional interactive tuning), while the bulk lane always remains FEC-free — no proxy-facing switch or protocol alias can override either lane's mode.

## Protocol

### TCP variant

<div style="background-color: white">
  
  ![protocol](img/protocol.tcp.drawio.png)
</div>

- Request header includes the address to the next hop, so that the Proxy Server can connect to the next hop on the fly
- Response header includes the ok/error message from the proxy that creates this response
- Steps to fill the stream:
  1. Proxy Client writes all the request headers to the stream
  1. Each Proxy Server consumes the responsible request header and writes the response header to the stream
  1. Proxy Client consumes all the response headers
  1. If no error, Proxy Client reads/writes the payload from/to the stream

### UDP variant

UDP client treats each UDP datagram as a stream. Each datagram carries a wire kind and a 128-bit flow ID that select the request form:

```
0: [kind | 128-bit flow ID | encrypted route header | payload]   # routed request
1: [kind | the same flow ID | payload]                           # compact request
```

- Kind 0 (routed request) carries the encrypted route header; kind 1 (compact request) omits it and reuses the flow ID.
- Each nested proxy layer uses the same flow ID and independently consumes exactly one kind/ID/header layer, so a chain of N proxies nests N layers and each proxy strips one.
- A normal UDP proxy listener keys its conntrack state exactly by (downstream address, wire flow ID); the authenticated upstream route is relay metadata learned from the first routed packet of the flow, not part of the conntrack key. The flow ID is the listener's actual conntrack identity.
- Access-server and SOCKS5 UDP have no wire flow ID: they remain route-keyed by (downstream address, destination), so one client can address multiple destinations.
- Compact form is used only after the client successfully decodes every proxy response layer (route confirmation). A compact packet can address an existing flow but cannot create route state: if it arrives after the flow timed out, the new unrouted flow is silently dropped so a later full routed request recreates it.
- Compact mode stays fresh only while both the monotonic clock (`Instant`) and wall clock (`SystemTime`) elapsed times are below `UDP_FLOW_TIMEOUT`; wall-clock rollback also forces a return to the full routed form. Expiration of either clock restores full routed requests.
- Inside a reverse tunnel's length-delimited UDP mux flow, each mux datagram carries the same kind/ID/header framing, and the first datagram of a mux flow must be a routed request.
