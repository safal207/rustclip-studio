//! A local, append-only causal journal.
//!
//! `seq` and `previous_hash` describe the journal's append order. They do not
//! assert that one business event caused the next. Causal dependencies are
//! explicit, typed parent references; wall clocks never establish causality.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::models::{CausalEvent, Project};

const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const RELATIONS: &[&str] = &["dependency", "hypothesis", "recovery", "supersedes"];
const CLAIM_LEVELS: &[&str] = &[
    "OBSERVATION",
    "DERIVED",
    "MODEL_OUTPUT",
    "HYPOTHESIS",
    "CORRECTION",
    "INTENT",
];

#[derive(Debug, Clone, Serialize)]
pub struct Integrity {
    pub valid: bool,
    pub count: usize,
    pub head_hash: String,
    pub error: Option<String>,
}

/// Hash the complete event with an empty `hash` field. This is a deterministic
/// integrity digest, not a signature or a proof that observations are true.
pub fn seal(event: &mut CausalEvent) -> Result<()> {
    if let Some(confidence) = event.confidence {
        if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
            bail!("confidence must be a finite value between 0 and 1");
        }
    }
    event.hash = event_digest(event)?;
    Ok(())
}

fn event_digest(event: &CausalEvent) -> Result<String> {
    let mut value = serde_json::to_value(event)?;
    value
        .as_object_mut()
        .context("event must serialize as an object")?
        .insert("hash".into(), Value::String(String::new()));
    let mut bytes = Vec::new();
    canonical_json(&value, &mut bytes)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// Sort every object's keys, including objects nested inside arrays. Array
/// order is meaningful and is therefore preserved.
fn canonical_json(value: &Value, output: &mut Vec<u8>) -> Result<()> {
    match value {
        Value::Object(object) => {
            output.push(b'{');
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort_unstable();
            for (index, key) in keys.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)?;
                output.push(b':');
                canonical_json(&object[*key], output)?;
            }
            output.push(b'}');
        }
        Value::Array(array) => {
            output.push(b'[');
            for (index, element) in array.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                canonical_json(element, output)?;
            }
            output.push(b']');
        }
        _ => serde_json::to_writer(output, value)?,
    }
    Ok(())
}

fn parse_time(value: &str, field: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value).with_context(|| format!("invalid {field}: {value}"))
}

/// Verify the complete global journal before interpreting a filtered view.
/// Local append order is checked independently from timestamp order, so valid
/// clock skew and evidence recorded after the fact are accepted.
pub fn verify(events: &[CausalEvent]) -> Integrity {
    let error = verify_inner(events).err().map(|error| format!("{error:#}"));
    Integrity {
        valid: error.is_none(),
        count: events.len(),
        head_hash: events
            .last()
            .map(|event| event.hash.clone())
            .unwrap_or_else(|| GENESIS_HASH.into()),
        error,
    }
}

fn verify_inner(events: &[CausalEvent]) -> Result<()> {
    let mut known: HashMap<&str, &CausalEvent> = HashMap::new();
    let mut previous_hash = GENESIS_HASH;
    for (index, event) in events.iter().enumerate() {
        let expected_seq = u64::try_from(index)?
            .checked_add(1)
            .context("sequence overflow")?;
        if event.seq != expected_seq {
            bail!(
                "event {}: sequence must be {expected_seq}, got {}",
                event.id,
                event.seq
            );
        }
        for (field, value) in [
            ("id", &event.id),
            ("subject_id", &event.subject_id),
            ("operation_id", &event.operation_id),
            ("execution_id", &event.execution_id),
        ] {
            if value.trim().is_empty() {
                bail!("event {}: {field} must not be empty", event.seq);
            }
        }
        if known.contains_key(event.id.as_str()) {
            bail!("duplicate event id: {}", event.id);
        }
        if event.operation_id == event.execution_id {
            bail!(
                "event {}: operation_id and execution_id must be distinct",
                event.id
            );
        }
        if event.previous_hash != previous_hash {
            bail!(
                "event {}: previous_hash does not match the journal predecessor",
                event.id
            );
        }
        parse_time(&event.valid_at, "valid_at")?;
        parse_time(&event.recorded_at, "recorded_at")?;
        if !CLAIM_LEVELS.contains(&event.claim_level.as_str()) {
            bail!(
                "event {}: unknown claim_level {}",
                event.id,
                event.claim_level
            );
        }
        if !event.space.is_null() && !event.space.is_object() {
            bail!("event {}: space coordinates must be an object", event.id);
        }
        if let Some(confidence) = event.confidence {
            if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
                bail!(
                    "event {}: confidence must be finite and within 0..=1",
                    event.id
                );
            }
        }
        if event.hash != event_digest(event)? {
            bail!(
                "event {}: hash does not match its canonical content",
                event.id
            );
        }

        let mut parent_ids = HashSet::new();
        for parent in &event.parents {
            if !RELATIONS.contains(&parent.relation.as_str()) {
                bail!(
                    "event {}: unknown parent relation {}",
                    event.id,
                    parent.relation
                );
            }
            if !parent_ids.insert(parent.id.as_str()) {
                bail!("event {}: duplicate parent {}", event.id, parent.id);
            }
            let source = known.get(parent.id.as_str()).with_context(|| {
                format!(
                    "event {}: parent {} is missing or not earlier",
                    event.id, parent.id
                )
            })?;
            if parent.hash != source.hash {
                bail!("event {}: parent {} hash mismatch", event.id, parent.id);
            }
            if parent.relation == "supersedes" && source.subject_id != event.subject_id {
                bail!(
                    "event {}: supersedes parent must have the same subject",
                    event.id
                );
            }
        }
        if let Some(supersedes) = &event.supersedes {
            let replaced = known.get(supersedes.as_str()).with_context(|| {
                format!(
                    "event {}: superseded event {} is missing or not earlier",
                    event.id, supersedes
                )
            })?;
            if replaced.subject_id != event.subject_id {
                bail!(
                    "event {}: superseded event must have the same subject",
                    event.id
                );
            }
        }
        known.insert(&event.id, event);
        previous_hash = &event.hash;
    }
    Ok(())
}

fn checked_integrity(events: &[CausalEvent]) -> Result<Integrity> {
    let integrity = verify(events);
    if !integrity.valid {
        bail!(
            "journal integrity failed: {}",
            integrity.error.as_deref().unwrap_or("unknown error")
        );
    }
    Ok(integrity)
}

fn cutoff(as_of: Option<&str>) -> Result<Option<DateTime<FixedOffset>>> {
    as_of.map(|value| parse_time(value, "as_of")).transpose()
}

fn accepted(event: &CausalEvent, cut: Option<&DateTime<FixedOffset>>) -> bool {
    // Timestamps have already been validated by checked_integrity.
    cut.is_none_or(|cut| {
        DateTime::parse_from_rfc3339(&event.recorded_at).is_ok_and(|time| time <= *cut)
    })
}

fn ancestor_closure<'a>(
    eligible: &HashMap<&'a str, &'a CausalEvent>,
    mut selected: HashSet<&'a str>,
) -> (HashSet<&'a str>, BTreeSet<&'a str>) {
    let mut missing = BTreeSet::new();
    let mut pending: Vec<&str> = selected.iter().copied().collect();
    while let Some(id) = pending.pop() {
        let event = eligible[id];
        for ancestor in event
            .parents
            .iter()
            .map(|parent| parent.id.as_str())
            .chain(event.supersedes.as_deref())
        {
            if !eligible.contains_key(ancestor) {
                missing.insert(ancestor);
            } else if selected.insert(ancestor) {
                pending.push(ancestor);
            }
        }
    }
    (selected, missing)
}

/// Produce a read-only graph. A subject view includes its eligible ancestors,
/// including corrected events, without turning adjacent journal rows into
/// causal links. A knowledge-time cut never imports evidence recorded later.
pub fn view(events: &[CausalEvent], subject: Option<&str>, as_of: Option<&str>) -> Result<Value> {
    let integrity = checked_integrity(events)?;
    let cut = cutoff(as_of)?;
    let eligible: HashMap<&str, &CausalEvent> = events
        .iter()
        .filter(|event| accepted(event, cut.as_ref()))
        .map(|event| (event.id.as_str(), event))
        .collect();
    let selected: HashSet<&str> = eligible
        .values()
        .filter(|event| subject.is_none_or(|subject| event.subject_id == subject))
        .map(|event| event.id.as_str())
        .collect();
    let (selected, missing_parent_ids) = ancestor_closure(&eligible, selected);
    let nodes: Vec<_> = events
        .iter()
        .filter(|event| selected.contains(event.id.as_str()))
        .collect();
    // A BTreeSet gives a stable edge order and prevents duplicate supersession
    // edges when both the field and a typed parent record the same correction.
    let mut edge_set = BTreeSet::new();
    for event in &nodes {
        for parent in &event.parents {
            if selected.contains(parent.id.as_str()) {
                edge_set.insert((
                    parent.id.as_str(),
                    event.id.as_str(),
                    parent.relation.as_str(),
                ));
            }
        }
        if let Some(replaced) = event.supersedes.as_deref() {
            if selected.contains(replaced) {
                edge_set.insert((replaced, event.id.as_str(), "supersedes"));
            }
        }
    }
    let edges: Vec<_> = edge_set
        .into_iter()
        .map(|(source, target, relation)| json!({"source": source, "target": target, "relation": relation}))
        .collect();
    Ok(json!({
        "nodes": nodes,
        "edges": edges,
        "integrity": integrity,
        "as_of": as_of,
        "boundary_complete": missing_parent_ids.is_empty(),
        "missing_parent_ids": missing_parent_ids,
        "semantics": "Explicit typed parents define the causal partial order; timestamps do not. valid_at is fact time; recorded_at is knowledge time. seq and previous_hash establish local append integrity only. dependency records a prerequisite; hypothesis records an unproven influence, not measured performance causation. Supersession preserves prior evidence. as_of filters recorded_at, including ancestors; boundary_complete=false means the time cut excluded required parent evidence, so this view is not a complete causal proof. Replay is read-only; hashes are not signatures or truth guarantees."
    }))
}

/// Reconstruct the latest recorded project snapshot without executing any
/// rendering, network, or publication operation.
pub fn snapshot(
    events: &[CausalEvent],
    subject: &str,
    as_of: Option<&str>,
) -> Result<Option<Project>> {
    checked_integrity(events)?;
    let cut = cutoff(as_of)?;
    let eligible: HashMap<&str, &CausalEvent> = events
        .iter()
        .filter(|event| accepted(event, cut.as_ref()))
        .map(|event| (event.id.as_str(), event))
        .collect();
    for event in events.iter().rev() {
        if event.subject_id != subject || !accepted(event, cut.as_ref()) {
            continue;
        }
        if let Some(value) = event.observed.get("project") {
            let (_, missing) = ancestor_closure(&eligible, HashSet::from([event.id.as_str()]));
            if !missing.is_empty() {
                bail!("event {}: snapshot causal boundary is incomplete; parent knowledge falls after as_of: {}", event.id, missing.into_iter().collect::<Vec<_>>().join(", "));
            }
            let project: Project = serde_json::from_value(value.clone())
                .with_context(|| format!("event {}: invalid project snapshot", event.id))?;
            if project.id != subject {
                bail!(
                    "event {}: project snapshot belongs to another subject",
                    event.id
                );
            }
            return Ok(Some(project));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ParentRef, Profile, ProjectSpec, Scene, Voice};

    fn event(seq: u64, subject: &str, previous_hash: &str) -> CausalEvent {
        CausalEvent {
            seq,
            id: format!("event-{seq}"),
            subject_id: subject.into(),
            operation_id: format!("operation-{seq}"),
            execution_id: format!("execution-{seq}"),
            valid_at: "2026-10-03T10:00:00Z".into(),
            recorded_at: "2026-10-03T10:01:00Z".into(),
            from_state: "draft".into(),
            to_state: "ready".into(),
            reason: "test observation".into(),
            claim_level: "OBSERVATION".into(),
            spatial_scope: "local".into(),
            space: Value::Null,
            confidence: None,
            parents: Vec::new(),
            supersedes: None,
            expected: json!({}),
            observed: json!({}),
            evidence: json!({}),
            previous_hash: previous_hash.into(),
            hash: String::new(),
        }
    }

    fn append(events: &mut Vec<CausalEvent>, mut event: CausalEvent) {
        seal(&mut event).unwrap();
        events.push(event);
    }

    fn parent(event: &CausalEvent, relation: &str) -> ParentRef {
        ParentRef {
            id: event.id.clone(),
            hash: event.hash.clone(),
            relation: relation.into(),
        }
    }

    fn project(revision: u32) -> Project {
        Project {
            id: "project-a".into(),
            revision,
            created_at: "2026-10-03T10:00:00Z".into(),
            updated_at: "2026-10-03T10:00:00Z".into(),
            status: "draft".into(),
            spec: ProjectSpec {
                title: format!("revision {revision}"),
                description: String::new(),
                topic: String::new(),
                profile: Profile::Shorts,
                language: "ru".into(),
                voice: Voice::None,
                scenes: vec![Scene {
                    title: "scene".into(),
                    text: String::new(),
                    narration: String::new(),
                    duration_s: 2.0,
                    asset: None,
                }],
                music_asset: None,
                source_urls: Vec::new(),
                tags: Vec::new(),
            },
            artifact: None,
            error: None,
        }
    }

    #[test]
    fn canonical_digest_sorts_objects_recursively_and_ignores_existing_hash() {
        let mut a = event(1, "project-a", GENESIS_HASH);
        a.observed = serde_json::from_str(r#"{"z":[{"b":2,"a":1}],"a":true}"#).unwrap();
        let mut b = a.clone();
        b.hash = "previous computed value".into();
        b.observed = serde_json::from_str(r#"{"a":true,"z":[{"a":1,"b":2}]}"#).unwrap();
        seal(&mut a).unwrap();
        seal(&mut b).unwrap();
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.hash.len(), 64);
    }

    #[test]
    fn optional_space_preserves_legacy_hashes_and_requires_structured_coordinates() {
        let mut legacy = event(1, "project-a", GENESIS_HASH);
        seal(&mut legacy).unwrap();
        let raw = serde_json::to_value(&legacy).unwrap();
        assert!(raw.get("space").is_none());
        let decoded: CausalEvent = serde_json::from_value(raw).unwrap();
        assert!(verify(&[decoded]).valid);
        legacy.space = json!("unstructured coordinates");
        seal(&mut legacy).unwrap();
        assert!(!verify(&[legacy]).valid);
    }

    #[test]
    fn tamper_fails_closed_for_views_and_replay() {
        let mut events = Vec::new();
        append(&mut events, event(1, "project-a", GENESIS_HASH));
        events[0].reason = "changed after sealing".into();
        assert!(!verify(&events).valid);
        assert!(view(&events, Some("project-a"), None).is_err());
        assert!(snapshot(&events, "project-a", None).is_err());
    }

    #[test]
    fn missing_and_mutated_parent_digests_are_rejected_even_when_resealed() {
        let mut events = Vec::new();
        append(&mut events, event(1, "project-a", GENESIS_HASH));
        let mut child = event(2, "project-a", &events[0].hash);
        child.parents.push(parent(&events[0], "dependency"));
        append(&mut events, child);
        assert!(verify(&events).valid);
        events[1].parents[0].hash = GENESIS_HASH.into();
        seal(&mut events[1]).unwrap();
        assert!(verify(&events)
            .error
            .unwrap()
            .contains("parent event-1 hash mismatch"));
        events[1].parents[0].id = "missing".into();
        seal(&mut events[1]).unwrap();
        assert!(verify(&events)
            .error
            .unwrap()
            .contains("missing or not earlier"));
    }

    #[test]
    fn branches_merge_by_parents_and_unrelated_append_order_is_not_causal() {
        let mut events = Vec::new();
        append(&mut events, event(1, "trend", GENESIS_HASH));
        let mut left = event(2, "project-a", &events[0].hash);
        left.parents.push(parent(&events[0], "hypothesis"));
        left.claim_level = "HYPOTHESIS".into();
        append(&mut events, left);
        let mut right = event(3, "audio", &events[1].hash);
        right.parents.push(parent(&events[0], "dependency"));
        append(&mut events, right);
        let mut merge = event(4, "project-a", &events[2].hash);
        merge.parents = vec![
            parent(&events[1], "dependency"),
            parent(&events[2], "dependency"),
        ];
        append(&mut events, merge);
        let predecessor_hash = events[3].hash.clone();
        append(&mut events, event(5, "unrelated", &predecessor_hash));
        assert!(verify(&events).valid);
        let graph = view(&events, Some("project-a"), None).unwrap();
        assert_eq!(graph["nodes"].as_array().unwrap().len(), 4);
        assert_eq!(graph["edges"].as_array().unwrap().len(), 4);
        assert!(!graph["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"] == "event-2" && edge["target"] == "event-3"));
    }

    #[test]
    fn fact_clock_skew_does_not_reverse_causal_order() {
        let mut events = Vec::new();
        let mut first = event(1, "project-a", GENESIS_HASH);
        first.valid_at = "2026-10-04T12:00:00+03:00".into();
        append(&mut events, first);
        let mut child = event(2, "project-a", &events[0].hash);
        child.valid_at = "2026-10-02T08:00:00Z".into();
        child.recorded_at = "2026-10-03T09:59:00Z".into();
        child.parents.push(parent(&events[0], "dependency"));
        append(&mut events, child);
        assert!(verify(&events).valid);
    }

    #[test]
    fn corrections_preserve_historic_snapshots_and_delayed_evidence_uses_recorded_time() {
        let mut events = Vec::new();
        let mut original = event(1, "project-a", GENESIS_HASH);
        original.observed = json!({"project": project(1)});
        append(&mut events, original);
        let mut correction = event(2, "project-a", &events[0].hash);
        correction.valid_at = "2026-10-02T08:00:00Z".into();
        correction.recorded_at = "2026-10-04T11:00:00Z".into();
        correction.claim_level = "CORRECTION".into();
        correction.supersedes = Some(events[0].id.clone());
        correction.observed = json!({"project": project(2)});
        append(&mut events, correction);
        assert_eq!(
            snapshot(&events, "project-a", Some("2026-10-03T12:00:00Z"))
                .unwrap()
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            snapshot(&events, "project-a", None)
                .unwrap()
                .unwrap()
                .revision,
            2
        );
        let history = view(&events, Some("project-a"), None).unwrap();
        assert_eq!(history["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(history["edges"][0]["relation"], "supersedes");
        assert_eq!(
            view(&events, None, Some("2026-10-03T12:00:00Z")).unwrap()["nodes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn duplicate_parents_cross_subject_supersession_and_bad_sequence_are_rejected() {
        let mut events = Vec::new();
        append(&mut events, event(1, "trend", GENESIS_HASH));
        let mut next = event(2, "project-a", &events[0].hash);
        next.parents = vec![
            parent(&events[0], "dependency"),
            parent(&events[0], "hypothesis"),
        ];
        append(&mut events, next);
        assert!(verify(&events).error.unwrap().contains("duplicate parent"));
        events[1].parents.clear();
        events[1].supersedes = Some(events[0].id.clone());
        seal(&mut events[1]).unwrap();
        assert!(verify(&events).error.unwrap().contains("same subject"));
        events[1].supersedes = None;
        events[1].seq = 3;
        seal(&mut events[1]).unwrap();
        assert!(verify(&events)
            .error
            .unwrap()
            .contains("sequence must be 2"));
    }

    #[test]
    fn empty_journal_and_invalid_cutoff_are_handled() {
        let integrity = verify(&[]);
        assert!(integrity.valid);
        assert_eq!(integrity.head_hash, GENESIS_HASH);
        assert!(view(&[], None, Some("not a time")).is_err());
        assert!(snapshot(&[], "project-a", None).unwrap().is_none());
    }

    #[test]
    fn skewed_recorded_clocks_mark_cut_boundaries_and_prevent_incomplete_replay() {
        let mut events = Vec::new();
        let mut evidence = event(1, "trend", GENESIS_HASH);
        evidence.recorded_at = "2026-10-04T10:00:00Z".into();
        append(&mut events, evidence);
        let mut target = event(2, "project-a", &events[0].hash);
        target.parents.push(parent(&events[0], "dependency"));
        target.observed = json!({"project": project(1)});
        append(&mut events, target);
        assert!(verify(&events).valid);
        let graph = view(&events, Some("project-a"), Some("2026-10-03T12:00:00Z")).unwrap();
        assert_eq!(graph["boundary_complete"], false);
        assert_eq!(graph["missing_parent_ids"], json!(["event-1"]));
        assert_eq!(graph["nodes"].as_array().unwrap().len(), 1);
        assert!(snapshot(&events, "project-a", Some("2026-10-03T12:00:00Z")).is_err());
        assert!(snapshot(&events, "project-a", None).unwrap().is_some());
    }

    #[test]
    fn confidence_is_optional_but_never_nonfinite_or_out_of_range() {
        let mut candidate = event(1, "project-a", GENESIS_HASH);
        candidate.claim_level = "HYPOTHESIS".into();
        seal(&mut candidate).unwrap();
        assert!(verify(&[candidate.clone()]).valid);
        for confidence in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            candidate.confidence = Some(confidence);
            assert!(seal(&mut candidate).is_err());
            assert!(!verify(&[candidate.clone()]).valid);
        }
    }
}
