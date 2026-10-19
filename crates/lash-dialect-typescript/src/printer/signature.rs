//! A declared function's parameter list when captions name its parameters
//! or the fields of its types: one parameter a line, each after its JSDoc,
//! and an inline object type for each parameter and result whose fields
//! carry captions, written from the entry's signature.

use lash_kernel_doc::{
    self as k, CaptionTarget, FieldPath, FieldRoot, FieldTarget, ParamTarget, Type,
};

use super::{Renderer, property_identifier};

impl Renderer<'_> {
    /// Writes the parameters of `function`, declared as `name`, and the
    /// annotation of its result when a caption names one of its fields.
    /// An entry's captions name its signature's parameters, which the
    /// printed function takes only when its own are those; otherwise they
    /// stay unspelled and are reported dropped.
    pub(super) fn signature(
        &mut self,
        document: &k::Document,
        name: &k::Name,
        function: &k::Function,
    ) {
        let entry = document.entries.get(name);
        let spelled = entry.is_none_or(|entry| {
            entry
                .signature
                .params
                .iter()
                .map(|param| &param.name)
                .eq(function.params.iter())
        });
        if !spelled
            || !self
                .out
                .holds_caption(|target| target.function() == Some(name))
        {
            self.parameters(&function.params);
            return;
        }
        self.out.write("(\n");
        self.indent += 1;
        for (index, param) in function.params.iter().enumerate() {
            self.out.write(&"  ".repeat(self.indent));
            if let Some(caption) = self.out.take_caption(ParamTarget {
                function: name.clone(),
                param: param.clone(),
            }) {
                self.caption(&caption);
            }
            self.name(param);
            let root = FieldRoot::Param(param.clone());
            if let Some(declared) = entry.and_then(|entry| entry.signature.params.get(index))
                && self.holds_fields(name, &root)
            {
                self.out.write(": ");
                self.ts_type(&declared.ty, name, &root, &mut Vec::new());
            }
            self.out.write(",\n");
        }
        self.indent -= 1;
        self.out.write(&"  ".repeat(self.indent));
        self.out.write(")");
        if let Some(entry) = entry
            && self.holds_fields(name, &FieldRoot::Result)
        {
            self.out.write(": Promise<");
            self.ts_type(
                &entry.signature.result,
                name,
                &FieldRoot::Result,
                &mut Vec::new(),
            );
            self.out.write(">");
        }
        self.out.write(" ");
    }

    fn holds_fields(&self, function: &k::Name, root: &FieldRoot) -> bool {
        self.out.holds_caption(|target| {
            matches!(target, CaptionTarget::Field(field)
                if field.function == *function && field.path.root == *root)
        })
    }

    /// `ty` as a TypeScript type, each record field after the caption
    /// `path` and its name give it.
    fn ts_type(&mut self, ty: &Type, function: &k::Name, root: &FieldRoot, path: &mut Vec<String>) {
        match ty {
            Type::Any | Type::Json | Type::Handle(_) => self.out.write("unknown"),
            Type::Null => self.out.write("null"),
            Type::Absent => self.out.write("undefined"),
            Type::Bool => self.out.write("boolean"),
            Type::Int | Type::Float | Type::Number => self.out.write("number"),
            Type::Text => self.out.write("string"),
            Type::Bytes => self.out.write("Uint8Array"),
            Type::Timestamp => self.out.write("Date"),
            Type::Error => self.out.write("Error"),
            Type::Function(_) => self.out.write("((...args: unknown[]) => unknown)"),
            Type::Enum(members) if members.is_empty() => self.out.write("never"),
            Type::Enum(members) => {
                for (index, member) in members.iter().enumerate() {
                    if index > 0 {
                        self.out.write(" | ");
                    }
                    self.out
                        .write(&serde_json::Value::from(member.as_str()).to_string());
                }
            }
            Type::Tuple(members) => {
                self.out.write("[");
                for (index, member) in members.iter().enumerate() {
                    if index > 0 {
                        self.out.write(", ");
                    }
                    self.ts_type(member, function, root, path);
                }
                self.out.write("]");
            }
            Type::List(item) => {
                self.out.write("Array<");
                self.ts_type(item, function, root, path);
                self.out.write(">");
            }
            Type::Set(item) => {
                self.out.write("Set<");
                self.ts_type(item, function, root, path);
                self.out.write(">");
            }
            Type::Task(item) => {
                self.out.write("Promise<");
                self.ts_type(item, function, root, path);
                self.out.write(">");
            }
            Type::Map(map) => {
                self.out.write("Map<");
                self.ts_type(&map.key, function, root, path);
                self.out.write(", ");
                self.ts_type(&map.value, function, root, path);
                self.out.write(">");
            }
            Type::Union(members) => {
                for (index, member) in members.iter().enumerate() {
                    if index > 0 {
                        self.out.write(" | ");
                    }
                    self.ts_type(member, function, root, path);
                }
            }
            Type::Record(record) => {
                if record.fields.is_empty() && record.rest.is_none() {
                    self.out.write("{}");
                    return;
                }
                self.out.write("{\n");
                self.indent += 1;
                for field in &record.fields {
                    self.out.write(&"  ".repeat(self.indent));
                    path.push(field.name.clone());
                    if let Some(caption) = self.out.take_caption(FieldTarget {
                        function: function.clone(),
                        path: FieldPath {
                            root: root.clone(),
                            fields: path.clone(),
                        },
                    }) {
                        self.caption(&caption);
                    }
                    if property_identifier(&field.name) {
                        self.out.write(&field.name);
                    } else {
                        self.out
                            .write(&serde_json::Value::from(field.name.as_str()).to_string());
                    }
                    self.out.write(if field.optional { "?: " } else { ": " });
                    self.ts_type(&field.ty, function, root, path);
                    self.out.write(";\n");
                    path.pop();
                }
                if let Some(rest) = &record.rest {
                    self.out.write(&"  ".repeat(self.indent));
                    self.out.write("[key: string]: ");
                    self.ts_type(rest, function, root, path);
                    self.out.write(";\n");
                }
                self.indent -= 1;
                self.out.write(&"  ".repeat(self.indent));
                self.out.write("}");
            }
        }
    }
}
