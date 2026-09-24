//! The engine laws that run only on the in-process effect host. Every store
//! law and every engine law another leg covers runs on the SQLite, PostgreSQL
//! or Restate legs; these wait for the Restate test engine (FIG-3665).
mod effect;
mod registrations;
