//! Whole-call scratch leases precede the allocation events they can overlap.
use lash_kernel_doc::{
    NativeError, NativeResourceContract as Contract, NativeResourceEvent as Event,
    NativeResourceHolding as Holding, NativeResourceOrigin as Origin, NativeResourceOwner as Owner,
    NativeResourceRelease as Release,
};

pub(crate) fn scratch(contract: &mut Contract, bytes: u64) -> Result<(), NativeError> {
    fn shift(owner: &mut Owner) -> Result<(), NativeError> {
        owner.0 = owner
            .0
            .checked_add(1)
            .ok_or(NativeError::Malformed("native logical owner overflow"))?;
        Ok(())
    }
    fn holding(value: &mut Holding) -> Result<(), NativeError> {
        if let Origin::Fresh(owner) = &mut value.origin {
            shift(owner)?;
        }
        Ok(())
    }
    fn release(value: &mut Release) -> Result<(), NativeError> {
        if let Release::Before(owner) = value {
            shift(owner)?;
        }
        Ok(())
    }
    holding(&mut contract.output)?;
    for event in &mut contract.events {
        match event {
            Event::Entity {
                owner,
                members,
                release: end,
                ..
            } => {
                shift(owner)?;
                if let Some(end) = end {
                    release(end)?;
                }
                for member in members {
                    holding(member)?;
                }
            }
            Event::Scratch {
                owner,
                release: end,
                ..
            } => {
                shift(owner)?;
                release(end)?;
            }
        }
    }
    contract.events.insert(
        0,
        Event::Scratch {
            owner: Owner(0),
            bytes,
            release: Release::Exit,
        },
    );
    Ok(())
}
