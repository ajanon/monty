//! Conversions for resume payloads: the parent's answers to suspension
//! events (`ResumeCall`, `ResumeNameLookup`, `ResumeFutures`). A returned
//! value is an index into the arena the same message carries.

use monty_types::{
    ExtFunctionResult, FutureResults, MontyException, MontyObject, NameLookupResult,
    unstable::{self, FutureOutcome, MontyGraph, NodeId},
};

use crate::{
    convert::{ProtoConvertError, graph_or_empty, root_object},
    pb,
    wire::{WireArena, graph_error},
};

/// Splits a call result into its wire kind and the arena a `Return` value
/// indexes (`None` for the other arms).
#[must_use]
pub fn ext_result_to_proto(result: ExtFunctionResult) -> (pb::ExtFunctionResult, Option<WireArena>) {
    let (kind, values) = match result {
        ExtFunctionResult::Return(value) => {
            let (graph, root) = unstable::into_graph_parts(value);
            (
                pb::ext_function_result::Kind::ReturnValue(root.0),
                Some(WireArena::new(graph)),
            )
        }
        ExtFunctionResult::Error(exc) => (pb::ext_function_result::Kind::Error((&exc).into()), None),
        ExtFunctionResult::Future(call_id) => (pb::ext_function_result::Kind::Future(call_id), None),
        ExtFunctionResult::NotFound(name) => (pb::ext_function_result::Kind::NotFound(name), None),
    };
    (pb::ExtFunctionResult { kind: Some(kind) }, values)
}

/// Validates a decoded call result against the arena its message carried.
pub fn ext_result_from_proto(
    result: pb::ExtFunctionResult,
    values: Option<WireArena>,
) -> Result<ExtFunctionResult, ProtoConvertError> {
    let kind = result
        .kind
        .ok_or(ProtoConvertError::MissingField("ExtFunctionResult.kind"))?;
    match kind {
        pb::ext_function_result::Kind::ReturnValue(root) => {
            Ok(ExtFunctionResult::Return(root_object(values, root, "values")?))
        }
        pb::ext_function_result::Kind::Error(err) => Ok(ExtFunctionResult::Error(MontyException::try_from(err)?)),
        pb::ext_function_result::Kind::Future(call_id) => Ok(ExtFunctionResult::Future(call_id)),
        pb::ext_function_result::Kind::NotFound(name) => Ok(ExtFunctionResult::NotFound(name)),
        pb::ext_function_result::Kind::NotHandled(_) => Err(not_handled_out_of_context()),
    }
}

/// NotHandled has no monty equivalent: it resolves against the suspended OS
/// call, so the child intercepts it before conversion (see
/// `Child::handle_resume_call`); anywhere else it is out of context.
fn not_handled_out_of_context() -> ProtoConvertError {
    ProtoConvertError::InvalidValue {
        field: "ExtFunctionResult.kind",
        reason: "NotHandled is only valid answering a suspended OS call".to_owned(),
    }
}

impl From<NameLookupResult> for pb::ResumeNameLookup {
    fn from(result: NameLookupResult) -> Self {
        let (kind, values) = match result {
            NameLookupResult::Value(value) => {
                let (graph, root) = unstable::into_graph_parts(value);
                (pb::resume_name_lookup::Kind::Value(root.0), Some(WireArena::new(graph)))
            }
            NameLookupResult::Undefined => (pb::resume_name_lookup::Kind::Undefined(pb::Unit {}), None),
            NameLookupResult::Error(exc) => (pb::resume_name_lookup::Kind::Error((&exc).into()), None),
        };
        Self {
            values,
            kind: Some(kind),
        }
    }
}

impl TryFrom<pb::ResumeNameLookup> for NameLookupResult {
    type Error = ProtoConvertError;

    fn try_from(lookup: pb::ResumeNameLookup) -> Result<Self, ProtoConvertError> {
        let kind = lookup
            .kind
            .ok_or(ProtoConvertError::MissingField("ResumeNameLookup.kind"))?;
        match kind {
            pb::resume_name_lookup::Kind::Value(root) => Ok(Self::Value(root_object(
                lookup.values,
                root,
                "ResumeNameLookup.values",
            )?)),
            pb::resume_name_lookup::Kind::Undefined(_) => Ok(Self::Undefined),
            pb::resume_name_lookup::Kind::Error(err) => Ok(Self::Error(MontyException::try_from(err)?)),
        }
    }
}

/// Merges every returned value into one arena and points each `ReturnValue`
/// into it.
#[must_use]
pub fn future_results_to_proto(results: Vec<(u32, ExtFunctionResult)>) -> pb::ResumeFutures {
    let mut values = MontyGraph::new();
    let results = results
        .into_iter()
        .map(|(call_id, result)| {
            let kind = match result {
                ExtFunctionResult::Return(value) => {
                    let (graph, root) = unstable::into_graph_parts(value);
                    let offset = values.merge(graph);
                    pb::ext_function_result::Kind::ReturnValue(root.0 + offset)
                }
                ExtFunctionResult::Error(exc) => pb::ext_function_result::Kind::Error((&exc).into()),
                ExtFunctionResult::Future(id) => pb::ext_function_result::Kind::Future(id),
                ExtFunctionResult::NotFound(name) => pb::ext_function_result::Kind::NotFound(name),
            };
            pb::FutureResult {
                call_id,
                result: Some(pb::ExtFunctionResult { kind: Some(kind) }),
            }
        })
        .collect();
    pb::ResumeFutures {
        results,
        values: Some(WireArena::new(values)),
    }
}

/// Converts wire future results into a batch for `ResolveFutures::resume_batch`.
///
/// The arena stays whole and is imported once on resume, so a value several
/// results reach costs the message rather than the result count times the
/// value. Bundled hosts send each result's own nodes, keeping copies per call.
pub fn future_results_from_proto(
    results: impl IntoIterator<Item = pb::FutureResult>,
    values: Option<WireArena>,
) -> Result<FutureResults, ProtoConvertError> {
    let graph = graph_or_empty(values)?;
    let results = results
        .into_iter()
        .map(|fr| {
            let result = fr
                .result
                .ok_or(ProtoConvertError::MissingField("FutureResult.result"))?;
            let kind = result
                .kind
                .ok_or(ProtoConvertError::MissingField("ExtFunctionResult.kind"))?;
            Ok((fr.call_id, future_outcome_from_proto(kind)?))
        })
        .collect::<Result<Vec<_>, ProtoConvertError>>()?;
    unstable::future_results_from_parts(graph, results).map_err(|err| graph_error(&err))
}

/// One wire result as a batch outcome; a returned value stays an index into
/// the batch's arena.
fn future_outcome_from_proto(kind: pb::ext_function_result::Kind) -> Result<FutureOutcome, ProtoConvertError> {
    match kind {
        pb::ext_function_result::Kind::ReturnValue(root) => Ok(FutureOutcome::Return(NodeId(root))),
        pb::ext_function_result::Kind::Error(err) => Ok(FutureOutcome::Error(MontyException::try_from(err)?)),
        pb::ext_function_result::Kind::Future(call_id) => Ok(FutureOutcome::Future(call_id)),
        pb::ext_function_result::Kind::NotFound(name) => Ok(FutureOutcome::NotFound(name)),
        pb::ext_function_result::Kind::NotHandled(_) => Err(not_handled_out_of_context()),
    }
}

/// The result of a `ResumeCall` reply, read with its arena by
/// [`ext_result_from_proto`].
pub fn resume_call_from_proto(call: pb::ResumeCall) -> Result<ExtFunctionResult, ProtoConvertError> {
    let result = call
        .result
        .ok_or(ProtoConvertError::MissingField("ResumeCall.result"))?;
    ext_result_from_proto(result, call.values)
}

impl From<MontyObject> for pb::ResumeCall {
    /// A `ResumeCall` returning `value`; the caller sets `call_id`.
    fn from(value: MontyObject) -> Self {
        let (result, values) = ext_result_to_proto(ExtFunctionResult::Return(value));
        Self {
            call_id: 0,
            result: Some(result),
            values,
        }
    }
}
