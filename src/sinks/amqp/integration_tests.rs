use super::*;
use crate::{
    amqp::await_connection,
    config::{SinkConfig, SinkContext},
    shutdown::ShutdownSignal,
    sinks::amqp::channel::new_channel_pool,
    template::{Template, UnsignedIntTemplate},
    test_util::{
        components::{run_and_assert_sink_compliance, SINK_TAGS},
        random_lines_with_stream, random_string,
    },
    SourceSender,
};
use config::AmqpPropertiesConfig;
use futures::StreamExt;
use std::{collections::HashSet, time::Duration};
use vector_lib::{config::LogNamespace, event::LogEvent};

pub fn make_config() -> AmqpSinkConfig {
    let mut config = AmqpSinkConfig {
        exchange: Template::try_from("it").unwrap(),
        ..Default::default()
    };
    let user = std::env::var("AMQP_USER").unwrap_or_else(|_| "guest".to_string());
    let pass = std::env::var("AMQP_PASSWORD").unwrap_or_else(|_| "guest".to_string());
    let host = std::env::var("AMQP_HOST").unwrap_or_else(|_| "rabbitmq".to_string());
    let vhost = std::env::var("AMQP_VHOST").unwrap_or_else(|_| "%2f".to_string());
    config.connection.connection_string = format!("amqp://{user}:{pass}@{host}:5672/{vhost}");
    config
}

#[tokio::test]
async fn healthcheck() {
    crate::test_util::trace_init();
    let exchange = format!("test-{}-exchange", random_string(10));

    let mut config = make_config();
    config.exchange = Template::try_from(exchange.as_str()).unwrap();
    await_connection(&config.connection).await;
    let channels = new_channel_pool(&config).unwrap();
    super::config::healthcheck(channels).await.unwrap();
}

#[tokio::test]
async fn amqp_happy_path_plaintext() {
    crate::test_util::trace_init();

    amqp_happy_path().await;
}

#[tokio::test]
async fn amqp_round_trip_plaintext() {
    crate::test_util::trace_init();

    amqp_round_trip().await;
}

async fn amqp_happy_path() {
    let mut config = make_config();
    let exchange = format!("test-{}-exchange", random_string(10));
    config.exchange = Template::try_from(exchange.as_str()).unwrap();
    let queue = format!("test-{}-queue", random_string(10));

    await_connection(&config.connection).await;
    let (_conn, channel) = config.connection.connect().await.unwrap();
    let exchange_opts = lapin::options::ExchangeDeclareOptions {
        auto_delete: true,
        ..Default::default()
    };
    channel
        .exchange_declare(
            &exchange,
            lapin::ExchangeKind::Fanout,
            exchange_opts,
            lapin::types::FieldTable::default(),
        )
        .await
        .unwrap();

    let cx = SinkContext::default();
    let (sink, healthcheck) = config.build(cx).await.unwrap();
    healthcheck.await.expect("Health check failed");

    // prepare consumer
    let queue_opts = lapin::options::QueueDeclareOptions {
        auto_delete: true,
        ..Default::default()
    };
    channel
        .queue_declare(&queue, queue_opts, lapin::types::FieldTable::default())
        .await
        .unwrap();

    channel
        .queue_bind(
            &queue,
            &exchange,
            "",
            lapin::options::QueueBindOptions::default(),
            lapin::types::FieldTable::default(),
        )
        .await
        .unwrap();

    let consumer = format!("test-{}-consumer", random_string(10));
    let mut consumer = channel
        .basic_consume(
            &queue,
            &consumer,
            lapin::options::BasicConsumeOptions::default(),
            lapin::types::FieldTable::default(),
        )
        .await
        .unwrap();

    let num_events = 1000;
    let (input, events) = random_lines_with_stream(100, num_events, None);
    run_and_assert_sink_compliance(sink, events, &SINK_TAGS).await;

    // loop instead of iter so we can set a timeout
    let mut failures = 0;
    let mut out = Vec::new();
    while failures < 10 && out.len() < input.len() {
        if let Ok(Some(try_msg)) =
            tokio::time::timeout(Duration::from_secs(10), consumer.next()).await
        {
            let msg = try_msg.unwrap();
            let s = String::from_utf8_lossy(msg.data.as_slice()).into_owned();

            let msg_priority = *msg.properties.priority();
            assert_eq!(msg_priority, None);

            out.push(s);
        } else {
            failures += 1;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    assert_eq!(out.len(), input.len());

    let input: HashSet<String> = HashSet::from_iter(input);
    let out: HashSet<String> = HashSet::from_iter(out);
    assert_eq!(out, input);
}

async fn amqp_round_trip() {
    let mut config = make_config();
    let exchange = format!("test-{}-exchange", random_string(10));
    config.exchange = Template::try_from(exchange.as_str()).unwrap();
    let queue = format!("test-{}-queue", random_string(10));

    await_connection(&config.connection).await;
    let (_conn, channel) = config.connection.connect().await.unwrap();
    let exchange_opts = lapin::options::ExchangeDeclareOptions {
        auto_delete: true,
        ..Default::default()
    };
    channel
        .exchange_declare(
            &exchange,
            lapin::ExchangeKind::Fanout,
            exchange_opts,
            lapin::types::FieldTable::default(),
        )
        .await
        .unwrap();

    let cx = SinkContext::default();
    let (amqp_sink, healthcheck) = config.build(cx).await.unwrap();
    healthcheck.await.expect("Health check failed");

    let source_cfg = crate::sources::amqp::AmqpSourceConfig {
        connection: config.connection.clone(),
        queue: queue.clone(),
        consumer: format!("test-{}-amqp-source", random_string(10)),
        log_namespace: Some(true),
        acknowledgements: true.into(),
        ..Default::default()
    };
    let (tx, rx) = SourceSender::new_test();
    let amqp_source = crate::sources::amqp::amqp_source(
        &source_cfg,
        ShutdownSignal::noop(),
        tx,
        LogNamespace::Legacy,
        true,
    )
    .await
    .unwrap();

    // prepare server
    let queue_opts = lapin::options::QueueDeclareOptions {
        auto_delete: true,
        ..Default::default()
    };
    channel
        .queue_declare(&queue, queue_opts, lapin::types::FieldTable::default())
        .await
        .unwrap();

    channel
        .queue_bind(
            &queue,
            &exchange,
            "",
            lapin::options::QueueBindOptions::default(),
            lapin::types::FieldTable::default(),
        )
        .await
        .unwrap();

    let _source_fut = tokio::spawn(amqp_source);

    // Have sink publish events
    let events_fut = async move {
        let num_events = 1000;
        let (_, events) = random_lines_with_stream(100, num_events, None);
        run_and_assert_sink_compliance(amqp_sink, events, &SINK_TAGS).await;
        num_events
    };
    let nb_events_published = tokio::spawn(events_fut).await.unwrap();
    let output = crate::test_util::collect_n(rx, 1000).await;

    assert_eq!(output.len(), nb_events_published);
}

async fn amqp_priority_with_template(
    template: &str,
    event_field_priority: Option<u8>,
    expected_priority: Option<u8>,
) {
    let mut config = make_config();
    let exchange = format!("test-{}-exchange", random_string(10));
    config.exchange = Template::try_from(exchange.as_str()).unwrap();
    config.properties = Some(AmqpPropertiesConfig {
        priority: Some(UnsignedIntTemplate::try_from(template).unwrap()),
        ..Default::default()
    });

    await_connection(&config.connection).await;
    let (_conn, channel) = config.connection.connect().await.unwrap();
    let exchange_opts = lapin::options::ExchangeDeclareOptions {
        auto_delete: true,
        ..Default::default()
    };
    channel
        .exchange_declare(
            &exchange,
            lapin::ExchangeKind::Fanout,
            exchange_opts,
            lapin::types::FieldTable::default(),
        )
        .await
        .unwrap();

    let cx = SinkContext::default();
    let (sink, healthcheck) = config.build(cx).await.unwrap();
    healthcheck.await.expect("Health check failed");

    // prepare consumer
    let queue = format!("test-{}-queue", random_string(10));
    let queue_opts = lapin::options::QueueDeclareOptions {
        auto_delete: true,
        ..Default::default()
    };
    let queue_args = {
        let mut args = lapin::types::FieldTable::default();
        args.insert(
            lapin::types::ShortString::from("x-max-priority"),
            lapin::types::AMQPValue::ShortInt(10), // Maximum priority value
        );
        args
    };
    channel
        .queue_declare(&queue, queue_opts, queue_args)
        .await
        .unwrap();

    channel
        .queue_bind(
            &queue,
            &exchange,
            "",
            lapin::options::QueueBindOptions::default(),
            lapin::types::FieldTable::default(),
        )
        .await
        .unwrap();

    let consumer = format!("test-{}-consumer", random_string(10));
    let mut consumer = channel
        .basic_consume(
            &queue,
            &consumer,
            lapin::options::BasicConsumeOptions::default(),
            lapin::types::FieldTable::default(),
        )
        .await
        .unwrap();

    // Send a single event with a priority defined in the event
    let input = random_string(100);
    let event = {
        let mut event = LogEvent::from_str_legacy(&input);
        if let Some(priority) = event_field_priority {
            event.insert("priority", priority);
        }
        event
    };

    let events = futures::stream::iter(vec![event]);
    run_and_assert_sink_compliance(sink, events, &SINK_TAGS).await;

    if let Ok(Some(try_msg)) = tokio::time::timeout(Duration::from_secs(10), consumer.next()).await
    {
        let msg = try_msg.unwrap();
        let msg_priority = *msg.properties.priority();
        let output = String::from_utf8_lossy(msg.data.as_slice()).into_owned();

        assert_eq!(msg_priority, expected_priority);
        assert_eq!(output, input);
    } else {
        panic!("Did not consume message in time.");
    }
}

#[tokio::test]
async fn amqp_priority_template_variable() {
    crate::test_util::trace_init();

    amqp_priority_with_template("{{ priority }}", Some(5), Some(5)).await;
}

#[tokio::test]
async fn amqp_priority_template_constant() {
    crate::test_util::trace_init();

    amqp_priority_with_template("5", None, Some(5)).await;
}

#[tokio::test]
async fn amqp_priority_template_out_of_bounds() {
    crate::test_util::trace_init();

    amqp_priority_with_template("100000", None, Some(u8::MAX)).await;
}
