#![cfg(all(test, feature = "sources-generator"))]

use super::controller::ControllerStatistics;
use crate::{
    assert_within,
    event::{metric::MetricValue, Event},
    metrics::{capture_metrics, get_controller, init as metrics_init},
    sinks::{
        util::{retries2::RetryLogic, service2::TowerRequestConfig, BatchSettings, VecBuffer},
        Healthcheck, RouterSink,
    },
    sources::generator::GeneratorConfig,
    test_util::{block_on, runtime, shutdown_on_idle, stats::LevelTimeHistogram},
    topology::{
        self,
        config::{self, DataType, SinkConfig, SinkContext},
    },
};
use core::task::Context;
use futures::{
    compat::Future01CompatExt,
    future::{pending, BoxFuture},
};
use futures01::{future, Sink};
use serde::Serialize;
use snafu::Snafu;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio01::timer::Delay;
use tower03::Service;

#[derive(Copy, Clone, Debug, Default, Serialize)]
struct TestParams {
    // The delay is the base time every request takes return.
    #[serde(default)]
    delay: Duration,

    // The concurrency scale is the rate at which requests' delay
    // increases at higher concurrency levels.
    #[serde(default)]
    concurrency_scale: f64,

    // The number of outstanding requests at which requests will return
    // with an error.
    #[serde(default)]
    concurrency_defer: usize,

    // The number of outstanding requests at which requests will be dropped.
    #[serde(default)]
    concurrency_drop: usize,
}

#[derive(Debug, Default, Serialize)]
struct TestConfig {
    request: TowerRequestConfig,
    params: TestParams,

    // The statistics collected by running a test must be local to that
    // test and retained past the completion of the topology. So, they
    // are created by `Default` and may be cloned to retain a handle.
    #[serde(skip)]
    stats: Arc<Mutex<Statistics>>,
    // Oh, the horror!
    #[serde(skip)]
    controller_stats: Arc<Mutex<Arc<Mutex<ControllerStatistics>>>>,
}

#[typetag::serialize(name = "test")]
impl SinkConfig for TestConfig {
    fn build(&self, cx: SinkContext) -> Result<(RouterSink, Healthcheck), crate::Error> {
        let batch = BatchSettings::default().events(1).bytes(9999).timeout(9999);
        let request = self.request.unwrap_with(&TowerRequestConfig::default());
        let sink = request
            .batch_sink(
                TestRetryLogic,
                TestSink::new(self),
                VecBuffer::new(batch.size),
                batch.timeout,
                cx.acker(),
            )
            .sink_map_err(|e| panic!("Fatal test sink error: {}", e));
        let healthcheck = future::ok(());

        // Dig deep to get at the internal controller statistics
        let stats = sink
            .get_ref()
            .get_ref()
            .get_ref()
            .get_ref()
            .get_ref()
            .controller
            .stats
            .clone();
        *self.controller_stats.lock().unwrap() = stats;

        Ok((Box::new(sink), Box::new(healthcheck)))
    }

    fn input_type(&self) -> DataType {
        DataType::Any
    }

    fn sink_type(&self) -> &'static str {
        "test"
    }

    fn typetag_deserialize(&self) {
        unimplemented!("not intended for use in real configs")
    }
}

#[derive(Clone, Debug)]
struct TestSink {
    stats: Arc<Mutex<Statistics>>,
    params: TestParams,
}

impl TestSink {
    fn new(config: &TestConfig) -> Self {
        Self {
            stats: config.stats.clone(),
            params: config.params,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Response {
    Ok,
}

impl crate::sinks::util::sink::Response for Response {}

// The TestSink service doesn't actually do anything with the data, it
// just delays a while depending on the configured parameters and then
// yields a result.
impl Service<Vec<Event>> for TestSink {
    type Response = Response;
    type Error = Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: Vec<Event>) -> Self::Future {
        let now = Instant::now();
        let mut stats = self.stats.lock().expect("Poisoned stats lock");
        stats.in_flight.adjust(1, now);
        let in_flight = stats.in_flight.level();

        let params = self.params;
        let delay = params
            .delay
            .mul_f64(1.0 + (in_flight - 1) as f64 * params.concurrency_scale);
        let delay = Delay::new(Instant::now() + delay).compat();

        if params.concurrency_drop > 0 && in_flight >= params.concurrency_drop {
            stats.in_flight.adjust(-1, now);
            Box::pin(pending())
        } else {
            let stats2 = self.stats.clone();
            Box::pin(async move {
                delay.await.expect("Delay failed!");
                let mut stats = stats2.lock().expect("Poisoned stats lock");
                let in_flight = stats.in_flight.level();
                stats.in_flight.adjust(-1, Instant::now());

                if params.concurrency_defer > 0 && in_flight >= params.concurrency_defer {
                    Err(Error::Deferred)
                } else {
                    Ok(Response::Ok)
                }
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Snafu)]
enum Error {
    Deferred,
}

#[derive(Clone, Copy)]
struct TestRetryLogic;

impl RetryLogic for TestRetryLogic {
    type Response = Response;
    type Error = Error;

    fn is_retriable_error(&self, _error: &Self::Error) -> bool {
        true
    }
}

#[derive(Debug, Default)]
struct Statistics {
    in_flight: LevelTimeHistogram,
}

#[derive(Debug)]
struct TestData {
    stats: Statistics,
    cstats: ControllerStatistics,
}

fn run_test(lines: usize, params: TestParams) -> TestData {
    metrics_init().ok();

    let test_config = TestConfig {
        request: TowerRequestConfig {
            in_flight_limit: Some(10),
            rate_limit_num: Some(9999),
            timeout_secs: Some(1),
            ..Default::default()
        },
        params,
        ..Default::default()
    };

    let stats = test_config.stats.clone();
    let cstats = test_config.controller_stats.clone();

    let mut config = config::Config::empty();
    config.add_source("in", GeneratorConfig::repeat(vec!["line 1".into()], lines));
    config.add_sink("out", &["in"], test_config);

    let mut rt = runtime();

    let (topology, _crash) = rt
        .block_on_std(topology::start(config, false))
        .expect("Failed to start topology");

    let controller = get_controller().unwrap();

    // Give time for the generator to start and queue all its data.
    std::thread::sleep(Duration::from_secs(1));
    block_on(topology.stop()).unwrap();
    shutdown_on_idle(rt);

    let stats = Arc::try_unwrap(stats)
        .expect("Failed to unwrap stats Arc")
        .into_inner()
        .expect("Failed to unwrap stats Mutex");

    let cstats = Arc::try_unwrap(cstats)
        .expect("Failed to unwrap controller_stats Arc")
        .into_inner()
        .expect("Failed to unwrap controller_stats Mutex");
    let cstats = Arc::try_unwrap(cstats)
        .expect("Failed to unwrap controller_stats Arc")
        .into_inner()
        .expect("Failed to unwrap controller_stats Mutex");

    let metrics = capture_metrics(&controller)
        .map(Event::into_metric)
        .map(|event| (event.name.clone(), event))
        .collect::<HashMap<_, _>>();
    // Ensure basic statistics are captured, don't actually examine them
    assert!(
        matches!(metrics.get("auto_concurrency_observed_rtt").unwrap().value,
                 MetricValue::Distribution { .. })
    );
    assert!(
        matches!(metrics.get("auto_concurrency_averaged_rtt").unwrap().value,
                 MetricValue::Distribution { .. })
    );
    assert!(
        matches!(metrics.get("auto_concurrency_limit").unwrap().value,
                 MetricValue::Distribution { .. })
    );
    assert!(
        matches!(metrics.get("auto_concurrency_in_flight").unwrap().value,
                 MetricValue::Distribution { .. })
    );

    eprintln!("observed_rtt {}", cstats.observed_rtt);
    eprintln!("averaged_rtt {}", cstats.averaged_rtt);
    eprintln!("concurrency_limit {}", cstats.concurrency_limit);
    eprintln!("in_flight {}", cstats.in_flight);

    TestData { stats, cstats }
}

#[test]
fn constant_link() {
    let results = run_test(
        500,
        TestParams {
            delay: Duration::from_millis(100),
            ..Default::default()
        },
    );

    let in_flight = results.stats.in_flight.stats().unwrap();
    // With a constant response time link and enough responses, the
    // limiter will ramp up to the maximum concurrency.
    assert_eq!(in_flight.max, 10, "{:#?}", results);
    // and will spend most of its time in the top half of the
    // concurrency range.
    assert_eq!(in_flight.mode, 10, "{:#?}", results);
    assert_within!(in_flight.mean, 7.0, 10.0, "{:#?}", results);

    let observed_rtt = results.cstats.observed_rtt.stats().unwrap();
    assert_within!(observed_rtt.min, 0.100, 0.110, "{:#?}", results);
    assert_within!(observed_rtt.max, 0.100, 0.110, "{:#?}", results);
    assert_within!(observed_rtt.mean, 0.100, 0.110, "{:#?}", results);
    let averaged_rtt = results.cstats.averaged_rtt.stats().unwrap();
    assert_within!(averaged_rtt.min, 0.100, 0.110, "{:#?}", results);
    assert_within!(averaged_rtt.max, 0.100, 0.110, "{:#?}", results);
    assert_within!(averaged_rtt.mean, 0.100, 0.110, "{:#?}", results);
    let concurrency_limit = results.cstats.concurrency_limit.stats().unwrap();
    assert_within!(concurrency_limit.max, 9, 10, "{:#?}", results);
    assert_within!(concurrency_limit.mode, 9, 10, "{:#?}", results);
    assert_within!(concurrency_limit.mean, 5.0, 10.0, "{:#?}", results);
    let in_flight = results.cstats.in_flight.stats().unwrap();
    assert_within!(in_flight.max, 9, 10, "{:#?}", results);
    assert_within!(in_flight.mode, 9, 10, "{:#?}", results);
    assert_within!(in_flight.mean, 7.0, 10.0, "{:#?}", results);
}

#[test]
fn defers_at_high_concurrency() {
    let results = run_test(
        500,
        TestParams {
            delay: Duration::from_millis(100),
            concurrency_defer: 5,
            ..Default::default()
        },
    );

    let in_flight = results.stats.in_flight.stats().unwrap();
    // With a constant time link that gives deferrals over a certain
    // concurrency, the limiter will ramp up to that concurrency and
    // then drop down repeatedly. Note that, due to the timing of the
    // adjustment, this may actually occasionally go over the error
    // limit above, but it will be rare.
    assert_within!(in_flight.max, 4, 6, "{:#?}", results);
    // Since the concurrency will drop down by half each time, the
    // average will be below this maximum.
    assert_within!(in_flight.mode, 2, 4, "{:#?}", results);
    assert_within!(in_flight.mean, 2.0, 4.0, "{:#?}", results);

    let observed_rtt = results.cstats.observed_rtt.stats().unwrap();
    assert_within!(observed_rtt.min, 0.100, 0.110, "{:#?}", results);
    assert_within!(observed_rtt.max, 0.100, 0.110, "{:#?}", results);
    assert_within!(observed_rtt.mean, 0.100, 0.110, "{:#?}", results);
    let averaged_rtt = results.cstats.averaged_rtt.stats().unwrap();
    assert_within!(averaged_rtt.min, 0.100, 0.110, "{:#?}", results);
    assert_within!(averaged_rtt.max, 0.100, 0.110, "{:#?}", results);
    assert_within!(averaged_rtt.mean, 0.100, 0.110, "{:#?}", results);
    let concurrency_limit = results.cstats.concurrency_limit.stats().unwrap();
    assert_within!(concurrency_limit.max, 5, 6, "{:#?}", results);
    assert_within!(concurrency_limit.mode, 2, 4, "{:#?}", results);
    assert_within!(concurrency_limit.mean, 2.0, 4.9, "{:#?}", results);
    let in_flight = results.cstats.in_flight.stats().unwrap();
    assert_within!(in_flight.max, 5, 6, "{:#?}", results);
    assert_within!(in_flight.mode, 2, 4, "{:#?}", results);
    assert_within!(in_flight.mean, 2.0, 4.0, "{:#?}", results);
}

#[test]
fn drops_at_high_concurrency() {
    let results = run_test(
        500,
        TestParams {
            delay: Duration::from_millis(100),
            concurrency_drop: 5,
            ..Default::default()
        },
    );

    let in_flight = results.stats.in_flight.stats().unwrap();
    // Since our internal framework doesn't track the "dropped"
    // requests, the values won't be representative of the actual number
    // of requests in flight (tracked below in the internal stats).
    assert_within!(in_flight.max, 4, 5, "{:#?}", results);
    assert_within!(in_flight.mode, 3, 4, "{:#?}", results);
    assert_within!(in_flight.mean, 1.5, 2.5, "{:#?}", results);

    let observed_rtt = results.cstats.observed_rtt.stats().unwrap();
    assert_within!(observed_rtt.min, 0.100, 0.110, "{:#?}", results);
    assert_within!(observed_rtt.max, 1.000, 1.010, "{:#?}", results);
    assert_within!(observed_rtt.mean, 0.150, 0.350, "{:#?}", results);
    let averaged_rtt = results.cstats.averaged_rtt.stats().unwrap();
    assert_within!(averaged_rtt.min, 0.100, 0.110, "{:#?}", results);
    assert_within!(averaged_rtt.max, 0.600, 1.010, "{:#?}", results);
    assert_within!(averaged_rtt.mean, 0.150, 0.350, "{:#?}", results);
    let concurrency_limit = results.cstats.concurrency_limit.stats().unwrap();
    assert_within!(concurrency_limit.mode, 2, 3, "{:#?}", results);
    assert_within!(concurrency_limit.mean, 3.5, 5.0, "{:#?}", results);
    let in_flight = results.cstats.in_flight.stats().unwrap();
    assert_within!(in_flight.mode, 1, 3, "{:#?}", results);
    assert_within!(in_flight.mean, 3.0, 5.0, "{:#?}", results);
}

#[test]
fn slow_link() {
    let results = run_test(
        500,
        TestParams {
            delay: Duration::from_millis(100),
            concurrency_scale: 1.0,
            ..Default::default()
        },
    );

    let in_flight = results.stats.in_flight.stats().unwrap();
    // With a link that slows down heavily as concurrency increases, the
    // limiter will keep the concurrency low (timing skews occasionally
    // has it reaching 3, but usually just 2),
    assert_within!(in_flight.max, 1, 3, "{:#?}", results);
    // and it will spend most of its time between 1 and 2.
    assert_within!(in_flight.mode, 1, 2, "{:#?}", results);
    assert_within!(in_flight.mean, 1.0, 2.0, "{:#?}", results);

    let observed_rtt = results.cstats.observed_rtt.stats().unwrap();
    assert_within!(observed_rtt.min, 0.100, 0.110, "{:#?}", results);
    assert_within!(observed_rtt.mean, 0.100, 0.310, "{:#?}", results);
    let averaged_rtt = results.cstats.averaged_rtt.stats().unwrap();
    assert_within!(averaged_rtt.min, 0.100, 0.110, "{:#?}", results);
    assert_within!(averaged_rtt.mean, 0.100, 0.310, "{:#?}", results);
    let concurrency_limit = results.cstats.concurrency_limit.stats().unwrap();
    assert_within!(concurrency_limit.mode, 1, 3, "{:#?}", results);
    assert_within!(concurrency_limit.mean, 1.0, 2.0, "{:#?}", results);
    let in_flight = results.cstats.in_flight.stats().unwrap();
    assert_within!(in_flight.max, 1, 3, "{:#?}", results);
    assert_within!(in_flight.mode, 1, 2, "{:#?}", results);
    assert_within!(in_flight.mean, 1.0, 2.0, "{:#?}", results);
}
