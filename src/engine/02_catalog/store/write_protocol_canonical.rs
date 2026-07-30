use crate::engine::catalog::model::WriteProtocol;

pub(crate) fn canonical_write_protocol(mut protocol: WriteProtocol) -> WriteProtocol {
    protocol.ready_indexes.sort_by(|left, right| {
        left.logical_id
            .cmp(&right.logical_id)
            .then_with(|| left.id.cmp(&right.id))
    });
    protocol
        .delta_sinks
        .sort_by(|left, right| left.transition_id.cmp(&right.transition_id));
    protocol
        .column_replacements
        .sort_by(|left, right| left.transition_id.cmp(&right.transition_id));
    protocol
        .constraint_checks
        .sort_by(|left, right| left.transition_id.cmp(&right.transition_id));
    protocol
}
