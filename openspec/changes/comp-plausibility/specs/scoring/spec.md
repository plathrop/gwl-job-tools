# Spec Delta

## ADDED Requirements

### Requirement: Implausible compensation is scored as unknown compensation

When a posting's compensation range exceeds the configured plausibility cap,
the scoring pipeline SHALL treat that compensation as unknown: the
compensation dimension SHALL be omitted from the composite, the remaining
dimensions' weights SHALL renormalize over what is present, and the
composite's breakdown SHALL state that the dimension was dropped for
implausibility — distinguishing it from a posting whose compensation is
merely unknown. A posting is never rejected, down-gated, or excluded for
implausible compensation; only its score changes.

The value compared against the cap SHALL be the top of the quoted range
when present, otherwise the bottom — the same value the compensation-floor
gate compares.

#### Scenario: A range above the cap drops the dimension

- **WHEN** a posting passes all gates and its compensation range's compared
  value exceeds the plausibility cap
- **THEN** the lead is scored with no compensation dimension, the remaining
  dimensions renormalize, and the breakdown identifies the compensation as
  implausible rather than unknown

#### Scenario: A range within the cap scores as before

- **WHEN** a posting's compensation range's compared value is at or below
  the cap
- **THEN** the compensation dimension contributes exactly as it does today

#### Scenario: Rescoring repairs previously inflated scores

- **WHEN** `rescore` re-evaluates a lead whose stored compensation range
  exceeds the cap
- **THEN** that lead's new score omits the compensation dimension, and the
  previously recorded score remains in the event log as history

### Requirement: Inverted compensation ranges are normalized before comparison

When a compensation range's minimum exceeds its maximum — an upstream data
error — the range SHALL be normalized by swapping the bounds before any
gate or scoring comparison consumes it, so a single malformed pair cannot
defeat both the floor gate and the plausibility check.

#### Scenario: A swapped range is compared correctly

- **WHEN** a posting's compensation range has a minimum greater than its
  maximum (e.g. min 400,000 / max 250,000)
- **THEN** the floor gate and the plausibility check both compare against
  400,000, as though the bounds had been recorded the right way round

### Requirement: The plausibility cap is config-derived and tunable

The cap SHALL be computed as the configured `compensation_ceiling`
multiplied by a `compensation_plausibility_multiplier` config value whose
default SHALL be 2.0. A multiplier below 1.0, or a non-finite value, SHALL
fail at config load with an error naming the key, before any lead is scored.
When no compensation ceiling is configured, the compensation dimension is
already absent and this rule has no effect.

#### Scenario: The default multiplier applies

- **WHEN** the operator configures a compensation ceiling and does not set
  `compensation_plausibility_multiplier`
- **THEN** the cap is twice the ceiling

#### Scenario: An operator raises the cap

- **WHEN** the operator sets `compensation_plausibility_multiplier` to a
  value of 1.0 or greater
- **THEN** the cap is that multiple of the ceiling, and ranges between the
  default cap and the raised cap are scored normally

#### Scenario: A nonsensical multiplier fails loudly

- **WHEN** the operator sets `compensation_plausibility_multiplier` below
  1.0 or to a non-finite value
- **THEN** the command fails at config load with an error naming the key,
  and no lead is scored under the invalid configuration
