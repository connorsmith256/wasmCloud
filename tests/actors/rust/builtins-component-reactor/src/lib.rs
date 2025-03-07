wit_bindgen::generate!({
    exports: {
        world: Actor,
        "wasi:http/incoming-handler": Actor,
    },
    with: {
        "wasi:io/streams@0.2.0-rc-2023-11-10": wasmcloud_actor::wasi::io::streams,
    }
});

use std::collections::BTreeMap;
use std::io::{Read, Write};

use serde::Deserialize;
use serde_json::json;
use wasi::http;
use wasi::io::poll::poll;
use wasi::sockets::{instance_network, network, tcp_create_socket, udp_create_socket};
use wasmcloud_actor::wasi::logging::logging;
use wasmcloud_actor::wasi::random::random;
use wasmcloud_actor::wasi::{blobstore, keyvalue};
use wasmcloud_actor::wasmcloud::bus::lattice::TargetEntity;
use wasmcloud_actor::wasmcloud::{bus, messaging};
use wasmcloud_actor::{
    debug, error, info, trace, warn, HostRng, InputStreamReader, OutputStreamWriter,
};

struct Actor;

impl exports::wasi::http::incoming_handler::Guest for Actor {
    fn handle(request: http::types::IncomingRequest, response_out: http::types::ResponseOutparam) {
        #[derive(Deserialize)]
        struct Request {
            min: u32,
            max: u32,
            port: u16,
            config_key: String,
        }

        assert!(matches!(request.method(), http::types::Method::Post));
        assert_eq!(request.path_with_query().as_deref(), Some("/foo?bar=baz"));
        assert!(request.scheme().is_none());
        // NOTE: Authority is lost in traslation to Smithy HttpRequest
        assert_eq!(request.authority(), None);
        let headers = request.headers();

        let header_entries = headers.entries().into_iter().collect::<BTreeMap<_, _>>();
        let mut header_iter = header_entries.clone().into_iter();

        assert_eq!(header_iter.next(), Some(("accept".into(), b"*/*".to_vec())));
        assert_eq!(headers.get(&String::from("accept")), vec![b"*/*"]);

        let (content_length_key, content_length_value) =
            header_iter.next().expect("`content-length` header missing");
        assert_eq!(content_length_key, "content-length");
        assert_eq!(
            headers.get(&String::from("content-length")),
            vec![content_length_value]
        );

        let (host_key, host_value) = header_iter.next().expect("`host` header missing");
        assert_eq!(host_key, "host");
        assert_eq!(headers.get(&String::from("host")), vec![host_value]);

        assert_eq!(
            header_iter.next(),
            Some(("test-header".into(), b"test-value".to_vec()))
        );
        assert_eq!(
            headers.get(&String::from("test-header")),
            vec![b"test-value"]
        );
        assert!(header_iter.next().is_none());

        let headers_clone = headers.clone();
        headers_clone
            .set(&String::from("foo"), &[b"bar".to_vec()])
            .expect("failed to set `foo` header");
        headers_clone
            .append(&String::from("foo"), &b"baz".to_vec())
            .expect("failed to append `foo` header");
        assert_eq!(
            headers_clone.get(&String::from("foo")),
            vec![b"bar", b"baz"]
        );
        headers_clone
            .delete(&String::from("foo"))
            .expect("failed to delete `foo` header");
        assert_eq!(
            headers_clone
                .entries()
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
            header_entries,
        );

        let request_body = request
            .consume()
            .expect("failed to get incoming request body");
        let Request {
            min,
            max,
            port,
            config_key,
        } = {
            let mut buf = vec![];
            let mut stream = request_body
                .stream()
                .expect("failed to get incoming request stream");
            InputStreamReader::from(&mut stream)
                .read_to_end(&mut buf)
                .expect("failed to read value from incoming request stream");
            serde_json::from_slice(&buf).expect("failed to decode request body")
        };
        let _trailers = http::types::IncomingBody::finish(request_body);

        logging::log(logging::Level::Trace, "trace-context", "trace");
        logging::log(logging::Level::Debug, "debug-context", "debug");
        logging::log(logging::Level::Info, "info-context", "info");
        logging::log(logging::Level::Warn, "warn-context", "warn");
        logging::log(logging::Level::Error, "error-context", "error");

        trace!(context: "trace-context", "trace");
        debug!(context: "debug-context", "debug");
        info!(context: "info-context", "info");
        warn!(context: "warn-context", "warn");
        error!(context: "error-context", "error");

        trace!("trace");
        debug!("debug");
        info!("info");
        warn!("warn");
        error!("error");

        let res = json!({
            "get_random_bytes": random::get_random_bytes(8),
            "get_random_u64": random::get_random_u64(),
            "guid": HostRng::generate_guid(),
            "random_32": HostRng::random32(),
            "random_in_range": HostRng::random_in_range(min, max),
            "long_value": "1234567890".repeat(1000),
            "config_value": wasmcloud::bus::guest_config::get(&config_key).expect("failed to get config value"),
            "all_config": wasmcloud::bus::guest_config::get_all().expect("failed to get all config values"),
        });
        eprintln!("response: `{res:?}`");
        let body = serde_json::to_vec(&res).expect("failed to encode response to JSON");
        let response = http::types::OutgoingResponse::new(http::types::Fields::new());
        let response_body = response
            .body()
            .expect("failed to get outgoing response body");
        {
            let mut stream = response_body
                .write()
                .expect("failed to get outgoing response stream");
            let mut w = OutputStreamWriter::from(&mut stream);
            w.write_all(&body)
                .expect("failed to write body to outgoing response stream");
            w.flush().expect("failed to flush outgoing response stream");
        }
        http::types::OutgoingBody::finish(response_body, None)
            .expect("failed to finish response body");
        http::types::ResponseOutparam::set(response_out, Ok(response));

        bus::lattice::set_target(
            Some(&TargetEntity::Link(Some("messaging".into()))),
            vec![bus::lattice::TargetInterface::wasmcloud_messaging_consumer()],
        );
        messaging::consumer::publish(&messaging::types::BrokerMessage {
            body: Some(body.clone()),
            reply_to: Some("noreply".into()),
            subject: "test-messaging-publish".into(),
        })
        .expect("failed to publish response");

        let messaging::types::BrokerMessage {
            body: response_body,
            reply_to,
            subject: _,
        } = messaging::consumer::request("test-messaging-request", Some(b"foo"), 1000)
            .expect("failed to request");
        assert_eq!(response_body.as_deref(), Some(b"bar".as_slice()));
        assert_eq!(reply_to, None);

        let responses = messaging::consumer::request_multi(
            "test-messaging-request-multi",
            Some(b"foo"),
            1000,
            1,
        )
        .expect("failed to request multi");
        let mut responses = responses.into_iter();
        match (responses.next(), responses.next()) {
            (
                Some(messaging::types::BrokerMessage {
                    body: response_body,
                    reply_to,
                    subject: _,
                }),
                None,
            ) => {
                assert_eq!(response_body.as_deref(), Some(b"bar".as_slice()));
                assert_eq!(reply_to, None);
            }
            (None, None) => panic!("no responses received"),
            _ => panic!("too many responses received"),
        }

        bus::lattice::set_target(
            Some(&TargetEntity::Link(Some("keyvalue".into()))),
            vec![bus::lattice::TargetInterface::wasi_keyvalue_readwrite()],
        );
        let foo_key = String::from("foo");
        let bucket = keyvalue::types::open_bucket("")
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to open empty bucket");
        keyvalue::readwrite::exists(bucket, &foo_key)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to check whether `foo` exists")
            .then_some(())
            .expect("`foo` does not exist");

        let foo_value = keyvalue::readwrite::get(bucket, &foo_key)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to get `foo`");

        let size = keyvalue::types::size(foo_value);
        assert_eq!(size, 3);

        let foo_value = keyvalue::types::incoming_value_consume_sync(foo_value)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to get incoming value buffer");
        assert_eq!(foo_value, b"bar");

        let foo_value = keyvalue::readwrite::get(bucket, &foo_key)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to get `foo`");
        let mut foo_stream = keyvalue::types::incoming_value_consume_async(foo_value)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to get incoming value stream");
        let mut foo_value = vec![];
        let n = InputStreamReader::from(&mut foo_stream)
            .read_to_end(&mut foo_value)
            .expect("failed to read value from keyvalue input stream");
        assert_eq!(n, 3);
        assert_eq!(foo_value, b"bar");

        keyvalue::readwrite::delete(bucket, &foo_key)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to delete `foo`");

        // NOTE: If https://github.com/WebAssembly/wasi-keyvalue/pull/18 is merged, this should not
        // return an error
        keyvalue::readwrite::exists(bucket, &foo_key)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect_err(
                "`exists` method should have returned an error for `foo` key, which was deleted",
            );

        let result_key = String::from("result");

        let result_value = keyvalue::types::new_outgoing_value();
        keyvalue::types::outgoing_value_write_body_sync(result_value, &body)
            .expect("failed to write outgoing value");
        keyvalue::readwrite::set(bucket, &result_key, result_value)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to set `result`");

        let result_value = keyvalue::readwrite::get(bucket, &result_key)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to get `result`");
        let result_value = keyvalue::types::incoming_value_consume_sync(result_value)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to get incoming value buffer");
        assert_eq!(result_value, body);

        let result_value = keyvalue::types::new_outgoing_value();
        let mut result_stream = keyvalue::types::outgoing_value_write_body_async(result_value)
            .expect("failed to get outgoing value output stream");
        let mut result_stream_writer = OutputStreamWriter::from(&mut result_stream);
        result_stream_writer
            .write_all(&body)
            .expect("failed to write result to keyvalue output stream");
        result_stream_writer
            .flush()
            .expect("failed to flush keyvalue output stream");
        keyvalue::readwrite::set(bucket, &result_key, result_value)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to set `result`");

        bus::lattice::set_target(
            Some(&TargetEntity::Link(Some("keyvalue".into()))),
            vec![bus::lattice::TargetInterface::wasi_keyvalue_atomic()],
        );
        let counter_key = String::from("counter");
        let value = keyvalue::atomic::increment(bucket, &counter_key, 1)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to increment `counter`");
        assert_eq!(value, 1);
        let value = keyvalue::atomic::increment(bucket, &counter_key, 41)
            .map_err(keyvalue::wasi_cloud_error::trace)
            .expect("failed to increment `counter`");
        assert_eq!(value, 42);

        // TODO: Verify return value when implemented for all hosts
        let _ = keyvalue::atomic::compare_and_swap(bucket, &counter_key, 42, 4242);
        let _ = keyvalue::atomic::compare_and_swap(bucket, &counter_key, 4242, 42);

        bus::lattice::set_target(
            Some(&TargetEntity::Link(Some("blobstore".into()))),
            vec![bus::lattice::TargetInterface::wasi_blobstore_blobstore()],
        );

        let container_name = String::from("container");
        assert!(!blobstore::blobstore::container_exists(&container_name)
            .expect("failed to check whether container exists"));
        let created_container = blobstore::blobstore::create_container(&container_name)
            .expect("failed to create container");
        assert!(blobstore::blobstore::container_exists(&container_name)
            .expect("failed to check whether container exists"));
        let got_container =
            blobstore::blobstore::get_container(&container_name).expect("failed to get container");

        let blobstore::container::ContainerMetadata { name, created_at } =
            blobstore::container::info(created_container)
                .expect("failed to get info of created container");
        assert_eq!(name, "container");
        assert!(created_at > 0);

        let got_info =
            blobstore::container::info(got_container).expect("failed to get info of got container");
        assert_eq!(got_info.name, "container");
        assert_eq!(got_info.created_at, created_at);
        // NOTE: At this point we should be able to assume that created container and got container are
        // indeed the same container
        blobstore::container::drop_container(got_container);

        assert_eq!(
            blobstore::container::name(created_container).expect("failed to get container name"),
            "container"
        );

        assert!(
            !blobstore::container::has_object(created_container, &result_key)
                .expect("failed to check whether `result` object exists")
        );
        // TODO: Assert that this succeeds once providers are compatible
        let _ = blobstore::container::delete_object(created_container, &result_key);

        let result_value = blobstore::types::new_outgoing_value();
        let mut result_stream = blobstore::types::outgoing_value_write_body(result_value)
            .expect("failed to get outgoing value output stream");
        let mut result_stream_writer = OutputStreamWriter::from(&mut result_stream);
        result_stream_writer
            .write_all(&body)
            .expect("failed to write result to blobstore output stream");
        result_stream_writer
            .flush()
            .expect("failed to flush blobstore output stream");
        blobstore::container::write_data(created_container, &result_key, result_value)
            .expect("failed to write `result`");

        // TODO: Expand blobstore testing procedure

        bus::lattice::set_target(
            Some(&TargetEntity::Actor(bus::lattice::ActorIdentifier::Alias(
                "unknown/alias".into(),
            ))),
            vec![bus::lattice::TargetInterface::new(
                "test-actors",
                "foobar",
                "foobar",
            )],
        );
        // TODO: Verify that this does not succeed, currently this invocation would trap
        //assert!(test_actors::foobar::foobar::foobar("foo").is_err());

        bus::lattice::set_target(
            Some(&TargetEntity::Actor(bus::lattice::ActorIdentifier::Alias(
                "foobar-component-command-preview2".into(),
            ))),
            vec![bus::lattice::TargetInterface::new(
                "test-actors",
                "foobar",
                "foobar",
            )],
        );
        assert_eq!(test_actors::foobar::foobar::foobar("foo"), "foobar");

        bus::lattice::set_target(
            Some(&TargetEntity::Link(Some("httpclient".into()))),
            vec![bus::lattice::TargetInterface::wasi_http_outgoing_handler()],
        );
        let request = http::types::OutgoingRequest::new(http::types::Fields::new());
        request
            .set_method(&http::types::Method::Put)
            .expect("failed to set request method");
        request
            .set_path_with_query(Some("/test"))
            .expect("failed to set request path with query");
        request
            .set_scheme(Some(&http::types::Scheme::Http))
            .expect("failed to set request scheme");
        request
            .set_authority(Some(&format!("localhost:{port}")))
            .expect("failed to set request authority");
        let request_body = request.body().expect("failed to get outgoing request body");
        {
            let mut stream = request_body
                .write()
                .expect("failed to get outgoing request stream");
            let mut w = OutputStreamWriter::from(&mut stream);
            w.write_all(b"test")
                .expect("failed to write `test` to outgoing request stream");
            w.flush().expect("failed to flush outgoing request stream");
        }
        http::types::OutgoingBody::finish(request_body, None)
            .expect("failed to finish sending request body");

        let response =
            http::outgoing_handler::handle(request, None).expect("failed to handle HTTP request");
        assert_eq!(poll(&[&response.subscribe()]), [0]);
        let response = response
            .get()
            .expect("HTTP request response missing")
            .expect("HTTP request response requested more than once")
            .expect("HTTP request failed");
        assert_eq!(response.status(), 200);
        // TODO: Assert headers
        _ = response.headers();
        let response_body = response
            .consume()
            .expect("failed to get incoming request body");
        {
            let mut buf = vec![];
            let mut stream = response_body
                .stream()
                .expect("failed to get HTTP request response stream");
            InputStreamReader::from(&mut stream)
                .read_to_end(&mut buf)
                .expect("failed to read value from HTTP request response stream");
            assert_eq!(buf, b"test");
        };
        let _trailers = http::types::IncomingBody::finish(response_body);

        let tcp4 = tcp_create_socket::create_tcp_socket(network::IpAddressFamily::Ipv4)
            .expect("failed to create an IPv4 TCP socket");
        let tcp6 = tcp_create_socket::create_tcp_socket(network::IpAddressFamily::Ipv6)
            .expect("failed to create an IPv6 TCP socket");
        let udp4 = udp_create_socket::create_udp_socket(network::IpAddressFamily::Ipv4)
            .expect("failed to create an IPv4 UDP socket");
        let udp6 = udp_create_socket::create_udp_socket(network::IpAddressFamily::Ipv6)
            .expect("failed to create an IPv6 UDP socket");
        tcp4.start_bind(
            &instance_network::instance_network(),
            network::IpSocketAddress::Ipv4(network::Ipv4SocketAddress {
                port: 0,
                address: (0, 0, 0, 0),
            }),
        )
        .expect_err("should not be able to bind to any IPv4 address on TCP");
        tcp6.start_bind(
            &instance_network::instance_network(),
            network::IpSocketAddress::Ipv6(network::Ipv6SocketAddress {
                port: 0,
                address: (0, 0, 0, 0, 0, 0, 0, 0),
                flow_info: 0,
                scope_id: 0,
            }),
        )
        .expect_err("should not be able to bind to any IPv6 address on TCP");
        udp4.start_bind(
            &instance_network::instance_network(),
            network::IpSocketAddress::Ipv4(network::Ipv4SocketAddress {
                port: 0,
                address: (0, 0, 0, 0),
            }),
        )
        .expect_err("should not be able to bind to any IPv4 address on UDP");
        udp6.start_bind(
            &instance_network::instance_network(),
            network::IpSocketAddress::Ipv6(network::Ipv6SocketAddress {
                port: 0,
                address: (0, 0, 0, 0, 0, 0, 0, 0),
                flow_info: 0,
                scope_id: 0,
            }),
        )
        .expect_err("should not be able to bind to any IPv6 address on UDP");
    }
}
