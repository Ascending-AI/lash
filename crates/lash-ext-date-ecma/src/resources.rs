//! Input-only obligations. Calendar calculations do not run a native or allocate a nursery.
use super::{Function, Operation, calendar, milliseconds, raise};
use lash_kernel_doc::{
    Args, Integer, LogicalHeldV1 as Cost, NativeError, NativeResourceContract as Contract,
    NativeResourceEntity as Entity, NativeResourceEvent as Event, NativeResourceExit as Exit,
    NativeResourceHolding as Holding, NativeResourceOrigin as Origin, NativeResourceOwner as Owner,
    NativeResourceRelease as Release, Value,
};

fn immutable(payload: u64) -> Holding {
    Holding {
        bytes: Cost::REFERENCE + payload,
        origin: Origin::Immutable,
    }
}
fn scratch(contract: &mut Contract, bytes: u64) {
    contract.events.push(Event::Scratch {
        owner: Owner(contract.events.len() as u32),
        bytes,
        release: Release::Exit,
    });
}

pub(super) fn contract(native: &Function, args: Args<'_>) -> Result<Option<Contract>, NativeError> {
    let mut result = match evaluate(native, args) {
        Err(NativeError::Refused { kind, message }) => Contract::refused(kind, &message)
            .ok_or(NativeError::Malformed("native logical cost overflow"))?,
        answer => answer?,
    };
    if matches!(
        native.operation,
        Operation::Milliseconds | Operation::Format
    ) && let Some(value) = args.get(0)
        && let Some(time) = args.timestamp(value)?
    {
        scratch(&mut result, Cost::REFERENCE + time.bits().div_ceil(8));
    }
    Ok(Some(result))
}

fn evaluate(native: &Function, args: Args<'_>) -> Result<Contract, NativeError> {
    let first = args
        .get(0)
        .ok_or_else(|| raise("type_error", "Missing argument"))?;
    let scalar =
        |payload| Contract::value(Exit::Returned, Cost::REFERENCE + payload, Origin::Immutable);
    match native.operation {
        Operation::Timestamp => {
            let Value::Float(time) = first else {
                return Err(raise("type_error", "Expected float"));
            };
            let clipped = calendar::time_clip(time.get());
            if clipped.is_nan() {
                let brand = immutable(2 * Cost::REFERENCE + 9 + 12);
                let built = immutable(32);
                return Ok(Contract {
                    exit: Exit::Returned,
                    output: Holding {
                        bytes: Cost::REFERENCE,
                        origin: Origin::Fresh(Owner(0)),
                    },
                    events: vec![Event::Entity {
                        owner: Owner(0),
                        kind: Entity::Record {
                            field_name_bytes: 12 + 12,
                        },
                        members: vec![brand, built],
                        release: None,
                    }],
                });
            }
            let decimal = format!("{clipped:.0}");
            let integer =
                Integer::parse(&decimal).map_err(|_| raise("RangeError", "Invalid time"))?;
            let nanoseconds = Integer::new(&*integer.as_bigint() * 1_000_000);
            let mut result = scalar(nanoseconds.bits().div_ceil(8));
            // The decimal conversion and intermediate integer overlap the result.
            scratch(
                &mut result,
                2 * Cost::REFERENCE + decimal.len() as u64 + integer.bits().div_ceil(8),
            );
            Ok(result)
        }
        Operation::Milliseconds => {
            milliseconds(first, &args)?;
            Ok(scalar(0))
        }
        Operation::Parse => {
            let source = args
                .text(first)?
                .ok_or_else(|| raise("type_error", "Expected text"))?;
            let source = source.whole()?;
            let mut result = match calendar::parse_date(&source) {
                Ok(_) | Err(calendar::IsoDateError::Invalid) => scalar(0),
                Err(calendar::IsoDateError::NonIso) => Contract::refused(
                    "TS_DATE_PARSE_NON_ISO",
                    "Use the ECMA ISO date-time or fixed UTC Date format",
                )
                .ok_or(NativeError::Malformed("native logical cost overflow"))?,
            };
            // Weekday-prefix probes are at most five bytes. A fixed-format
            // parser owns a token table and up to three bounded rendering texts.
            scratch(&mut result, Cost::REFERENCE + 5);
            if source.is_ascii() && source.len() <= 80 {
                let tokens = source.split(' ').count() as u64;
                scratch(
                    &mut result,
                    Cost::OBJECT
                        + tokens * Cost::REFERENCE
                        + source.len() as u64
                        + 3 * (Cost::REFERENCE + 80),
                );
            }
            Ok(result)
        }
        Operation::Format => {
            let time = milliseconds(first, &args)?;
            let format = args
                .get(1)
                .map(|format| args.text(format))
                .transpose()?
                .flatten()
                .ok_or_else(|| raise("type_error", "Expected format"))?;
            let text = match format.whole()?.as_ref() {
                "iso" => calendar::to_iso_string(time)
                    .ok_or_else(|| raise("RangeError", "Invalid time value"))?,
                "utc" => calendar::date_utc_string(time),
                "string" => calendar::date_to_string(time),
                "date" => calendar::date_date_string(time),
                "time" => calendar::date_time_string(time),
                "locale_date" => calendar::date_locale_date_string(time),
                "locale_time" => calendar::date_locale_time_string(time),
                "locale" => calendar::date_locale_string(time),
                _ => return Err(raise("type_error", "Unknown date format")),
            };
            let mut result = scalar(text.len() as u64);
            // Formatting owns a text buffer until the immutable result is installed.
            // The longest branch joins two calendar parts while building the
            // output. Each part (including an extended year) fits the result.
            scratch(&mut result, 3 * (Cost::REFERENCE + text.len() as u64));
            Ok(result)
        }
    }
}
