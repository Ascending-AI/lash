use lash::workflow::document::RegisteredFunction;

fn replace(mut function: RegisteredFunction, other: RegisteredFunction) {
    function.definition = other.definition;
    function.native = other.native;
}

fn main() { let _ = replace; }
