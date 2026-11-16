//! Read-only attribution of execution-core receipts; this module never chooses a route.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use hyper::HeaderMap;
use rayline_metrics::{MetricsSink, MetricsUpdate, RouterMetrics, new_request_id};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Deserialize)]
struct Deployment {
    id: String,
    kind: DeploymentKind,
    wire_model: String,
}

/// Where a deployment runs: this machine or Rayline Platform.
#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum DeploymentKind {
    Local,
    Cloud,
}

/// How long each recent local decision took, by route id, until its
/// receipt reads it. Holds the last [`DecisionTimes::CAPACITY`] only.
#[derive(Default)]
pub(super) struct DecisionTimes(Mutex<(HashMap<String, u64>, VecDeque<String>)>);

impl DecisionTimes {
    const CAPACITY: usize = 256;

    pub(super) fn record(&self, route_id: &str, ms: u64) {
        let mut times = self.0.lock().expect("decision times lock poisoned");
        let (by_route, order) = &mut *times;
        if by_route.insert(route_id.to_owned(), ms).is_none() {
            order.push_back(route_id.to_owned());
        }
        while order.len() > Self::CAPACITY {
            if let Some(oldest) = order.pop_front() {
                by_route.remove(&oldest);
            }
        }
    }

    fn take(&self, route_id: &str) -> Option<u64> {
        let mut times = self.0.lock().expect("decision times lock poisoned");
        let (by_route, order) = &mut *times;
        let ms = by_route.remove(route_id)?;
        order.retain(|id| id != route_id);
        Some(ms)
    }
}

#[derive(Default)]
pub(super) struct RoutingMetadata {
    deployments: HashMap<String, Deployment>,
    actions: HashMap<String, Vec<(Option<String>, String)>>,
    derived: HashMap<(String, String), Vec<String>>,
    policy: Option<String>,
    /// `decisions.model` of the core config.
    decisions_model: Option<String>,
    /// With local decisions, how long each took.
    local_decisions: Option<Arc<DecisionTimes>>,
}

impl RoutingMetadata {
    /// The attribution data in the core config. A deployment that does not
    /// parse, such as one whose `kind` is neither `local` nor `cloud`, is an
    /// error naming it.
    pub(super) fn from_config(config: &Value) -> Result<Self, String> {
        let mut metadata = Self::default();
        if let Some(deployments) = config.get("deployments").and_then(Value::as_array) {
            for (index, value) in deployments.iter().enumerate() {
                let deployment = serde_json::from_value::<Deployment>(value.clone())
                    .map_err(|error| format!("core config `deployments[{index}]`: {error}"))?;
                metadata
                    .deployments
                    .insert(deployment.id.clone(), deployment);
            }
        }
        for field in ["derived_actions", "bindings"] {
            if let Some(bindings) = config.get(field).and_then(Value::as_array) {
                for binding in bindings {
                    let Some(deployment) = binding.get("deployment_id").and_then(Value::as_str)
                    else {
                        continue;
                    };
                    if field == "derived_actions"
                        && let (Some(worker), Some(model)) = (
                            binding.get("worker").and_then(Value::as_str),
                            binding.get("model").and_then(Value::as_str),
                        )
                    {
                        metadata
                            .derived
                            .entry((worker.to_owned(), model.to_owned()))
                            .or_default()
                            .push(deployment.to_owned());
                    }
                    if field == "bindings"
                        && let Some(action) = binding.get("action_id").and_then(Value::as_str)
                    {
                        metadata
                            .actions
                            .entry(action.to_owned())
                            .or_default()
                            .push((
                                binding
                                    .get("worker")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                                deployment.to_owned(),
                            ));
                    }
                }
            }
        }
        metadata.policy = config
            .pointer("/decisions/routing/policy/preset")
            .and_then(Value::as_str)
            .map(str::to_owned);
        metadata.decisions_model = config
            .pointer("/decisions/model")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(metadata)
    }

    /// Decisions are made on this machine, timed by `times`.
    pub(super) fn decide_locally(&mut self, times: Arc<DecisionTimes>) {
        self.local_decisions = Some(times);
    }

    fn router_kind(&self) -> &'static str {
        if self.local_decisions.is_some() {
            "local"
        } else {
            "hosted"
        }
    }

    /// The router that decides, and its model, for `top`.
    pub(super) fn router_summary(&self) -> Value {
        serde_json::json!({"kind": self.router_kind(), "model": self.decisions_model})
    }

    /// Whether every deployment is Rayline Platform.
    pub(super) fn all_cloud(&self) -> bool {
        self.deployments
            .values()
            .all(|deployment| deployment.kind == DeploymentKind::Cloud)
    }

    /// The deployment that executed a receipt, when exactly one did, and
    /// where it ran, when every candidate runs in the same place.
    fn deployment(
        &self,
        worker: Option<&str>,
        action: Option<&str>,
        model: Option<&str>,
    ) -> (Option<&Deployment>, Option<DeploymentKind>) {
        if let Some(bindings) = action.and_then(|id| self.actions.get(id)) {
            let Some((_, deployment)) = bindings.first() else {
                return (None, None);
            };
            // A known action must agree with every binding and the receipt's worker.
            // Never fall through to derived attribution after contradictory evidence.
            if bindings.iter().any(|(bound_worker, candidate)| {
                candidate != deployment
                    || worker.is_some_and(|worker| bound_worker.as_deref() != Some(worker))
            }) {
                return (None, None);
            }
            return unique(self.deployments.get(deployment));
        }
        let Some(candidates) = worker
            .zip(model)
            .and_then(|(worker, model)| self.derived.get(&(worker.to_owned(), model.to_owned())))
        else {
            return (None, None);
        };
        let Some(first) = candidates.first() else {
            return (None, None);
        };
        // Multiple controls on one deployment are fine. Two deployments (one
        // model served over several protocols) name no endpoint, but still
        // name where it ran when they all run in the same place.
        if candidates.iter().all(|candidate| candidate == first) {
            return unique(self.deployments.get(first));
        }
        let mut kinds = candidates
            .iter()
            .map(|candidate| self.deployments.get(candidate).map(|d| d.kind));
        let kind = kinds.next().flatten();
        let same = kind.is_some() && kinds.all(|other| other == kind);
        (None, if same { kind } else { None })
    }

    pub(super) fn record(
        &self,
        observation: &RequestObservation,
        headers: &HeaderMap,
        instance: &str,
    ) {
        if header(headers, "x-rayline-instance-id").as_deref() != Some(instance) {
            tracing::debug!("hybrid routing receipt missing or belongs to another instance");
            return;
        }
        let executed_model = header(headers, "x-rayline-executed-model");
        let worker = header(headers, "x-rayline-executed-worker");
        let action = header(headers, "x-rayline-executed-action-id");
        let (deployment, kind) = match executed_model {
            Some(_) => self.deployment(
                worker.as_deref(),
                action.as_deref(),
                executed_model.as_deref(),
            ),
            None => (None, None),
        };
        if deployment.is_none() {
            tracing::debug!("hybrid routing receipt has no unambiguous deployment attribution");
        }
        observation
            .metrics
            .record(MetricsUpdate::ExecutionIdentity {
                request_id: observation.id.clone(),
                decision_model: header(headers, "x-rayline-selected-model"),
                executed_model,
                executed_worker: worker,
                executed_action_id: action,
                selected_action_id: header(headers, "x-rayline-action-id"),
                executed_control_id: header(headers, "x-rayline-executed-control-id"),
                wire_model: deployment.map(|deployment| deployment.wire_model.clone()),
            });
        // A receipt whose candidates don't all run in one place has no target.
        let target = kind.map(|kind| match kind {
            DeploymentKind::Local => "local",
            DeploymentKind::Cloud => "remote",
        });
        let route_id = header(headers, "x-rayline-route-id");
        let event = header(headers, "x-rayline-event");
        let decision_ms = self
            .local_decisions
            .as_ref()
            .zip(route_id.as_deref())
            .and_then(|(times, route_id)| times.take(route_id));
        observation.metrics.record(MetricsUpdate::RouteDecided {
            request_id: observation.id.clone(),
            route_id,
            target: target.map(str::to_owned),
            endpoint_id: deployment.map(|deployment| deployment.id.clone()),
            requested_model: None,
            policy: self.policy.clone(),
            // `kind/basis`: the decision's event is its kind, when it is
            // one the contract defines.
            decision_event: event
                .as_deref()
                .and_then(|event| event.split('/').next())
                .filter(|kind| rayline_arc_routes::EVENT_KINDS.contains(kind))
                .map(str::to_owned),
            task_class: event,
            router: Some(self.router_kind().to_owned()),
            decision_ms,
        });
    }
}

fn unique(deployment: Option<&Deployment>) -> (Option<&Deployment>, Option<DeploymentKind>) {
    (deployment, deployment.map(|deployment| deployment.kind))
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .filter(|value| {
            !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        })
        .map(str::to_owned)
}

/// Owned by the response body so a downstream disconnect closes its active row.
pub(super) struct RequestObservation {
    pub(super) id: String,
    pub(super) metrics: Arc<RouterMetrics>,
    finished: bool,
}

impl RequestObservation {
    pub(super) fn start(metrics: Arc<RouterMetrics>, body: &[u8]) -> Self {
        let id = new_request_id();
        // The gateway already buffers this request; retain only its short model ID.
        let requested_model = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|model| {
                        model.len() <= 256
                            && model.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || b"/-_.:#".contains(&byte)
                            })
                    })
                    .map(str::to_owned)
            });
        metrics.record(MetricsUpdate::RequestStarted {
            request_id: id.clone(),
            source: "hybrid".to_owned(),
            requested_model,
        });
        Self {
            id,
            metrics,
            finished: false,
        }
    }

    pub(super) fn record_classifier(&self, model: &str) {
        // A cloud pin: no routing decision, only the model that serves it.
        self.metrics.record(MetricsUpdate::ExecutionIdentity {
            request_id: self.id.clone(),
            decision_model: None,
            executed_model: Some(model.to_owned()),
            executed_worker: None,
            executed_action_id: None,
            selected_action_id: None,
            executed_control_id: None,
            wire_model: None,
        });
        self.metrics.record(MetricsUpdate::RouteDecided {
            request_id: self.id.clone(),
            route_id: None,
            target: Some("remote".to_owned()),
            endpoint_id: None,
            requested_model: None,
            policy: Some("classifier cloud pin".to_owned()),
            task_class: Some("sidecall".to_owned()),
            router: None,
            decision_ms: None,
            decision_event: None,
        });
    }

    pub(super) fn finish(&mut self, status: u16, error: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(error) = error {
            self.metrics.record(MetricsUpdate::RequestErrored {
                request_id: self.id.clone(),
                status_code: Some(status),
                error: error.to_owned(),
            });
        } else {
            self.metrics.record(MetricsUpdate::RequestCompleted {
                request_id: self.id.clone(),
                status_code: Some(status),
                input_tokens: None,
                output_tokens: None,
            });
        }
    }
}

impl Drop for RequestObservation {
    fn drop(&mut self) {
        if !self.finished {
            self.metrics.record(MetricsUpdate::RequestErrored {
                request_id: self.id.clone(),
                status_code: None,
                error: "request cancelled or transport failed".to_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn receipt(route_id: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (key, value) in [
            ("x-rayline-instance-id", "instance"),
            ("x-rayline-route-id", route_id),
            ("x-rayline-event", "tool_result/explicit"),
            ("x-rayline-executed-model", "glm"),
        ] {
            headers.insert(key, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn a_receipt_says_where_its_decision_was_made_how_long_it_took_and_its_event() {
        let config = json!({"decisions": {"model": "arc-swift-1.0"}});
        let mut metadata = RoutingMetadata::from_config(&config).unwrap();
        assert_eq!(
            metadata.router_summary(),
            json!({"kind": "hosted", "model": "arc-swift-1.0"})
        );
        let metrics = RouterMetrics::new("hybrid");
        let hosted = RequestObservation::start(metrics.clone(), b"{}");
        metadata.record(&hosted, &receipt("rt_1"), "instance");
        let row = &metrics.snapshot().active[0];
        assert_eq!(row.router.as_deref(), Some("hosted"));
        assert_eq!(row.decision_ms, None);
        assert_eq!(row.decision_event.as_deref(), Some("tool_result"));
        assert_eq!(row.task_class.as_deref(), Some("tool_result/explicit"));
        drop(hosted);

        let times = Arc::new(DecisionTimes::default());
        metadata.decide_locally(times.clone());
        assert_eq!(metadata.router_summary()["kind"], "local");
        times.record("rt_2", 38);
        let local = RequestObservation::start(metrics.clone(), b"{}");
        metadata.record(&local, &receipt("rt_2"), "instance");
        let row = &metrics.snapshot().active[0];
        assert_eq!(row.router.as_deref(), Some("local"));
        assert_eq!(row.decision_ms, Some(38));
        // Read once: the time is gone with its receipt.
        assert_eq!(times.take("rt_2"), None);

        // An event that isn't one of the contract's kinds is not shown.
        let mut odd = receipt("rt_3");
        odd.insert("x-rayline-event", "anything at all/x".parse().unwrap());
        let other = RequestObservation::start(metrics.clone(), b"{}");
        metadata.record(&other, &odd, "instance");
        let rows = metrics.snapshot().active;
        let row = rows
            .iter()
            .find(|row| row.task_class.as_deref() == Some("anything at all/x"));
        assert_eq!(row.expect("row").decision_event, None);
    }

    #[test]
    fn decision_times_keep_only_the_most_recent() {
        let times = DecisionTimes::default();
        for index in 0..=DecisionTimes::CAPACITY {
            times.record(&format!("rt_{index}"), index as u64);
        }
        assert_eq!(times.take("rt_0"), None);
        assert_eq!(times.take("rt_1"), Some(1));
        let last = DecisionTimes::CAPACITY;
        assert_eq!(times.take(&format!("rt_{last}")), Some(last as u64));
    }

    #[test]
    fn receipts_attribute_execution_not_selection_and_disconnect_closes_the_row() {
        let metadata = RoutingMetadata::from_config(&json!({
            "deployments": [{"id":"spark","kind":"local","wire_model":"served-glm"}],
            "bindings": [{"worker":"glm","action_id":"a1","deployment_id":"spark"}]
        }))
        .unwrap();
        let metrics = RouterMetrics::new("hybrid");
        let observation =
            RequestObservation::start(metrics.clone(), br#"{"model":"rayline-auto"}"#);
        let mut headers = HeaderMap::new();
        for (key, value) in [
            ("x-rayline-instance-id", "instance"),
            ("x-rayline-selected-model", "cloud-model"),
            ("x-rayline-executed-model", "glm"),
            ("x-rayline-executed-worker", "glm"),
            ("x-rayline-executed-action-id", "a1"),
        ] {
            headers.insert(key, value.parse().unwrap());
        }
        metadata.record(&observation, &headers, "instance");
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.active[0].target.as_deref(), Some("local"));
        assert_eq!(snapshot.active[0].executed_model.as_deref(), Some("glm"));
        assert_eq!(
            snapshot.active[0].decision_model.as_deref(),
            Some("cloud-model")
        );
        assert_eq!(snapshot.active[0].endpoint_id.as_deref(), Some("spark"));
        drop(observation);
        let snapshot = metrics.snapshot();
        assert!(snapshot.active.is_empty());
        assert_eq!(snapshot.recent[0].state, "error");
        assert_eq!(snapshot.totals.local_requests, 1);
    }

    #[test]
    fn unknown_or_conflicting_receipts_stay_unattributed() {
        for (config, worker, model, action) in [
            (
                json!({"derived_actions": [{"worker":"w","model":"m","deployment_id":"cloud"}]}),
                "w",
                "unknown-model",
                "unknown-action",
            ),
            (
                json!({"bindings": [{"worker":"w","action_id":"a1","deployment_id":"cloud"}]}),
                "different-worker",
                "m",
                "a1",
            ),
            (
                json!({"bindings": [{"worker":"w","action_id":"a1","deployment_id":"cloud"}, {"worker":"w","action_id":"a1","deployment_id":"local"}]}),
                "w",
                "m",
                "a1",
            ),
            (
                json!({"derived_actions": [{"worker":"w","model":"m","deployment_id":"cloud"}, {"worker":"w","model":"m","deployment_id":"local"}]}),
                "w",
                "m",
                "dynamic-action",
            ),
        ] {
            let mut config = config;
            config["deployments"] = json!([
                {"id":"cloud","kind":"cloud","wire_model":"cloud-wire"},
                {"id":"local","kind":"local","wire_model":"local-wire"}
            ]);
            let metadata = RoutingMetadata::from_config(&config).unwrap();
            let metrics = RouterMetrics::new("hybrid");
            let mut observation = RequestObservation::start(metrics.clone(), b"{}");
            let mut headers = HeaderMap::new();
            for (key, value) in [
                ("x-rayline-instance-id", "instance"),
                ("x-rayline-executed-worker", worker),
                ("x-rayline-executed-model", model),
                ("x-rayline-executed-action-id", action),
            ] {
                headers.insert(key, value.parse().unwrap());
            }
            metadata.record(&observation, &headers, "instance");
            observation.finish(200, None);
            let snapshot = metrics.snapshot();
            assert!(snapshot.recent[0].target.is_none(), "{config}");
            assert!(snapshot.recent[0].endpoint_id.is_none());
            assert_eq!(snapshot.totals.remote_requests, 0);
            assert_eq!(snapshot.totals.local_requests, 0);
        }
    }

    #[test]
    fn a_cloud_model_over_several_protocols_counts_as_remote_without_an_endpoint() {
        let metadata = RoutingMetadata::from_config(&json!({
            "deployments": [
                {"id":"m-messages","kind":"cloud","wire_model":"m"},
                {"id":"m-chat","kind":"cloud","wire_model":"m"}
            ],
            "derived_actions": [
                {"worker":"w","model":"m","deployment_id":"m-messages"},
                {"worker":"w","model":"m","deployment_id":"m-chat"}
            ]
        }))
        .unwrap();
        let metrics = RouterMetrics::new("hybrid");
        let mut observation = RequestObservation::start(metrics.clone(), b"{}");
        let mut headers = HeaderMap::new();
        for (key, value) in [
            ("x-rayline-instance-id", "instance"),
            ("x-rayline-executed-worker", "w"),
            ("x-rayline-executed-model", "m"),
            ("x-rayline-executed-action-id", "dynamic-action"),
        ] {
            headers.insert(key, value.parse().unwrap());
        }
        metadata.record(&observation, &headers, "instance");
        observation.finish(200, None);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.recent[0].target.as_deref(), Some("remote"));
        assert!(snapshot.recent[0].endpoint_id.is_none());
        assert_eq!(snapshot.totals.remote_requests, 1);
    }

    #[test]
    fn ambiguous_worker_and_wrong_instance_never_guess_a_target() {
        let metadata = RoutingMetadata::from_config(&json!({
            "deployments": [{"id":"a","kind":"local","wire_model":"m"},{"id":"b","kind":"cloud","wire_model":"m"}],
            "derived_actions": [{"worker":"w","deployment_id":"a"},{"worker":"w","deployment_id":"b"}]
        }))
        .unwrap();
        let (deployment, kind) = metadata.deployment(Some("w"), None, Some("m"));
        assert!(deployment.is_none() && kind.is_none());
        let metrics = RouterMetrics::new("hybrid");
        let observation = RequestObservation::start(metrics.clone(), b"{}");
        let mut headers = HeaderMap::new();
        headers.insert("x-rayline-instance-id", "old-instance".parse().unwrap());
        metadata.record(&observation, &headers, "new-instance");
        assert!(metrics.snapshot().active[0].target.is_none());
    }

    #[test]
    fn a_deployment_kind_other_than_local_or_cloud_is_an_error_naming_it() {
        let error = RoutingMetadata::from_config(&json!({
            "deployments": [{"id":"a","kind":"local","wire_model":"m"},{"id":"b","kind":"gpu","wire_model":"m"}]
        }))
        .err()
        .expect("an unknown kind is refused");
        assert!(error.contains("`deployments[1]`"), "{error}");
        assert!(error.contains("`gpu`"), "{error}");
    }
}
