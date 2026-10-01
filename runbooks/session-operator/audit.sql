CREATE TABLE operator_model_calls (
    marker TEXT NOT NULL,
    request_json TEXT NOT NULL
);
CREATE TABLE operator_terminal_writes (
    session_id TEXT NOT NULL,
    root TEXT NOT NULL,
    terminal_kind TEXT NOT NULL
);
CREATE TABLE operator_child_cancels (process_id TEXT NOT NULL);
CREATE FUNCTION operator_audit_terminal() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.terminal_kind IS NOT NULL AND
       ROW(OLD.terminal_kind, OLD.terminal_cause_json, OLD.terminal_at_ms, OLD.terminal_head_revision)
       IS DISTINCT FROM
       ROW(NEW.terminal_kind, NEW.terminal_cause_json, NEW.terminal_at_ms, NEW.terminal_head_revision) THEN
        INSERT INTO operator_terminal_writes VALUES (NEW.session_id, NEW.root, NEW.terminal_kind);
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER operator_audit_terminal AFTER UPDATE ON lash_session_roots
    FOR EACH ROW EXECUTE FUNCTION operator_audit_terminal();
CREATE FUNCTION operator_audit_cancel() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.cancel_requested_at_ms IS NOT NULL AND OLD.cancel_requested_at_ms IS NULL THEN
        INSERT INTO operator_child_cancels VALUES (NEW.process_id);
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER operator_audit_cancel AFTER UPDATE ON lash_processes
    FOR EACH ROW EXECUTE FUNCTION operator_audit_cancel();
