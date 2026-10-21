//! An arena natives run on outside any run: native laws and harnesses build
//! their inputs from data, call one native as the machine calls it, and read
//! its answer back as data.

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::heap::LogicalBytes;
use lash_kernel_doc::{
    Args, Bytes, Datum, ErrorDatum, NativeCall, NativeError, NativeFunction, NewObject, Nursery,
    Reservation, Retry, Timestamp, Value, WorkCounter,
};

use lash_vocabulary::data_limits::KernelDataLimits;
use lash_vocabulary::names::Name;

use super::Arena;

/// An arena natives are called on, under data limits.
#[derive(Debug)]
pub struct Bench {
    arena: Arena,
    limits: KernelDataLimits,
}

/// What one call answered, and what it asked of the run's memory.
#[derive(Debug)]
pub struct Called {
    /// The answer as the run sees it: a call whose objects outgrow its room
    /// answers [`Retry::Room`], as the machine's admission of them would.
    pub answer: Result<Value, NativeError>,
    pub usage: Usage,
}

/// What a call asked of the run's memory.
#[derive(Debug, Default)]
pub struct Usage {
    /// Every reservation the call asked for, in order.
    pub reservations: Vec<Reservation>,
    /// What the call held when it returned.
    pub held: LogicalBytes,
    /// The bytes the capacity meter counts for what it built
    /// ([`Arena::install_bound`]).
    pub grown: u64,
    /// How many mutable objects it built.
    pub objects: usize,
}

impl Usage {
    /// What the reservations that were held come to.
    pub fn reserved(&self) -> u64 {
        self.reservations
            .iter()
            .filter(|reservation| reservation.held)
            .fold(0u64, |sum, reservation| {
                sum.saturating_add(reservation.values.saturating_mul(16))
                    .saturating_add(reservation.bytes)
            })
    }
}

impl Default for Bench {
    fn default() -> Self {
        Self::new()
    }
}

impl Bench {
    pub fn new() -> Self {
        Self::with_limits(KernelDataLimits::standard())
    }

    pub fn with_limits(limits: KernelDataLimits) -> Self {
        Self {
            arena: Arena::new(),
            limits,
        }
    }

    pub fn arena(&self) -> &Arena {
        &self.arena
    }

    pub fn arena_mut(&mut self) -> &mut Arena {
        &mut self.arena
    }

    /// Builds `datum`, each mutable object a fresh one.
    ///
    /// # Errors
    ///
    /// A datum no run holds (an undecoded number, a function value), or one
    /// past the value depth bound.
    pub fn build(&mut self, datum: &Datum) -> Result<Value, NativeError> {
        Ok(match datum {
            Datum::Null => Value::Null,
            Datum::Absent => Value::Absent,
            Datum::Bool(value) => Value::Bool(*value),
            Datum::Int(value) => self.arena.int(value.clone()),
            Datum::Float(value) => Value::Float(*value),
            Datum::Text(value) => self.arena.text(value),
            Datum::Bytes(value) => self.arena.bytes(value.as_slice()),
            Datum::Timestamp(value) => self.arena.timestamp(value.nanoseconds.clone()),
            Datum::Function(name) => self.arena.function(*name),
            Datum::Handle(handle) => self.arena.host_handle(handle.clone()),
            Datum::Tuple(members) => {
                let members = self.all(members)?;
                self.arena
                    .tuple(members, self.limits.value_depth())
                    .map_err(|_| NativeError::Malformed("a datum passes the value depth"))?
            }
            Datum::List(items) => {
                let items = self.all(items)?;
                self.object(NewObject::List(items))?
            }
            Datum::Set(members) => {
                let members = self.all(members)?;
                self.object(NewObject::Set(members))?
            }
            Datum::Map(entries) => {
                let entries = entries
                    .iter()
                    .map(|(key, value)| Ok((self.build(key)?, self.build(value)?)))
                    .collect::<Result<_, NativeError>>()?;
                self.object(NewObject::Map(entries))?
            }
            Datum::Record(fields) => {
                let fields = fields
                    .iter()
                    .map(|(name, value)| Ok((self.arena.text(name), self.build(value)?)))
                    .collect::<Result<_, NativeError>>()?;
                self.object(NewObject::Record(fields))?
            }
            Datum::Error(error) => {
                let data = self.build(&error.data)?;
                self.arena.error(&error.kind, &error.message, data)
            }
            Datum::Number(_) => return Err(NativeError::Malformed("an undecoded number")),
            Datum::FunctionValue(_) => {
                return Err(NativeError::Malformed(
                    "a function value never enters a run",
                ));
            }
        })
    }

    fn all(&mut self, data: &[Datum]) -> Result<Vec<Value>, NativeError> {
        data.iter().map(|datum| self.build(datum)).collect()
    }

    fn object(&mut self, object: NewObject) -> Result<Value, NativeError> {
        let mut nursery = Nursery::new(self.arena.objects(), LogicalBytes(u64::MAX));
        let value = nursery.allocate(object);
        let new = nursery.into_new();
        self.install(new)?;
        Ok(value)
    }

    fn install(&mut self, new: Vec<(ArenaHandle, NewObject)>) -> Result<u64, NativeError> {
        let grown = self.arena.install_bound(&new);
        self.arena.install(new)?;
        Ok(grown)
    }

    /// Calls `native` on `args` with `room` to reserve and `counter` as its
    /// guard, and installs what it built when it answers a value or raises.
    pub fn call(
        &mut self,
        native: &dyn NativeFunction,
        args: &[Value],
        room: u64,
        counter: &mut WorkCounter,
    ) -> Called {
        let (answer, mut usage, new) = self.attempt(
            args,
            room,
            |args, new| native.call(NativeCall { args, new, counter }),
            |answer| matches!(answer, Ok(_) | Err(NativeError::Raised(_))),
        );
        if let Err(error) = self.keep(&mut usage, new) {
            return Called {
                answer: Err(error),
                usage,
            };
        }
        // The machine counts what a call built once it returns: objects
        // that outgrow the room end the run at its memory bound.
        let answer = match answer {
            Ok(_) | Err(NativeError::Raised(_)) if usage.grown > room => Err(Retry::Room {
                wanted: LogicalBytes(usage.grown),
            }
            .into()),
            answer => answer,
        };
        Called { answer, usage }
    }

    /// Runs `body` as a native call's body over `args`, with `room` to
    /// reserve, and installs whatever it built.
    ///
    /// # Errors
    ///
    /// The arena's refusal to install what it built.
    pub fn run<R>(
        &mut self,
        args: &[Value],
        room: u64,
        body: impl for<'c> FnOnce(Args<'c>, &mut Nursery<'c>) -> R,
    ) -> Result<(R, Usage), NativeError> {
        let (answer, mut usage, new) = self.attempt(args, room, body, |_| true);
        self.keep(&mut usage, new)?;
        Ok((answer, usage))
    }

    /// Runs `body`, and gives what it built when `kept` keeps its answer;
    /// otherwise its slots are given back.
    fn attempt<R>(
        &mut self,
        args: &[Value],
        room: u64,
        body: impl for<'c> FnOnce(Args<'c>, &mut Nursery<'c>) -> R,
        kept: impl FnOnce(&R) -> bool,
    ) -> (R, Usage, Option<Vec<(ArenaHandle, NewObject)>>) {
        let semantics = self.arena.semantics().with_limits(self.limits);
        let mut nursery = Nursery::recording(self.arena.objects(), LogicalBytes(room));
        let answer = body(
            Args::new(args, self.arena.objects(), &semantics),
            &mut nursery,
        );
        let usage = Usage {
            reservations: nursery.reservations().to_vec(),
            held: nursery.held(),
            ..Usage::default()
        };
        if kept(&answer) {
            (answer, usage, Some(nursery.into_new()))
        } else {
            nursery.discard();
            (answer, usage, None)
        }
    }

    fn keep(
        &mut self,
        usage: &mut Usage,
        new: Option<Vec<(ArenaHandle, NewObject)>>,
    ) -> Result<(), NativeError> {
        let Some(new) = new else {
            return Ok(());
        };
        usage.objects = new
            .iter()
            .filter(|(_, object)| {
                matches!(
                    object,
                    NewObject::List(_)
                        | NewObject::Map(_)
                        | NewObject::Set(_)
                        | NewObject::Record(_)
                )
            })
            .count();
        usage.grown = self.install(new)?;
        Ok(())
    }

    /// What `value` holds, as data.
    ///
    /// # Errors
    ///
    /// A value with no data form: a task, a closure, a reference.
    pub fn datum(&self, value: &Value) -> Result<Datum, NativeError> {
        let semantics = self.arena.semantics();
        let handle = |value: &Value| {
            value
                .handle()
                .ok_or(NativeError::Malformed("a value names no object"))
        };
        Ok(match value {
            Value::Null => Datum::Null,
            Value::Absent => Datum::Absent,
            Value::Bool(value) => Datum::Bool(*value),
            Value::Float(value) => Datum::Float(*value),
            Value::Int(_) | Value::BigInt(_) => Datum::Int(
                semantics
                    .integer(value)?
                    .ok_or(NativeError::Malformed("an integer names another body"))?,
            ),
            Value::Timestamp(_) | Value::BigTimestamp(_) => Datum::Timestamp(Timestamp {
                nanoseconds: semantics
                    .nanoseconds(value)?
                    .ok_or(NativeError::Malformed("a timestamp names another body"))?,
            }),
            Value::Text(_) | Value::LongText(_) => Datum::Text(semantics.text(value)?.to_owned()),
            Value::Function(handle) => Datum::Function(self.arena.callable(*handle)?),
            Value::Bytes(bytes) => Datum::Bytes(Bytes::new(semantics.bytes(*bytes)?.to_vec())),
            Value::Handle(handle) => match self.arena.body(*handle)? {
                Body::Handle(handle) => Datum::Handle(handle.clone()),
                _ => return Err(NativeError::Malformed("a handle names another body")),
            },
            Value::Tuple(tuple) => Datum::Tuple(self.each(semantics.sequence(*tuple)?)?),
            Value::List(list) => Datum::List(self.each(semantics.sequence(*list)?)?),
            Value::Set(set) => Datum::Set(
                semantics
                    .entries(*set)?
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| self.datum(&pair[0]))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Map(map) => Datum::Map(
                semantics
                    .entries(*map)?
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| Ok((self.datum(&pair[0])?, self.datum(&pair[1])?)))
                    .collect::<Result<_, NativeError>>()?,
            ),
            Value::Record(record) => Datum::Record(
                semantics
                    .entries(*record)?
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| Ok((semantics.text(&pair[0])?.to_owned(), self.datum(&pair[1])?)))
                    .collect::<Result<_, NativeError>>()?,
            ),
            Value::Error(_) => match self.arena.body(handle(value)?)? {
                Body::Error(error) => Datum::Error(Box::new(ErrorDatum {
                    kind: semantics.text(&error.kind)?.to_owned(),
                    message: semantics.text(&error.message)?.to_owned(),
                    data: self.datum(&error.data)?,
                })),
                _ => return Err(NativeError::Malformed("an error names another body")),
            },
            _ => return Err(NativeError::Malformed("a value with no data form")),
        })
    }

    fn each(&self, values: &[Value]) -> Result<Vec<Datum>, NativeError> {
        values.iter().map(|value| self.datum(value)).collect()
    }
}
