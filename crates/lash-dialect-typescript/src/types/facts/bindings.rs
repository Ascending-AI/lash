//! Contents used to type extracted bindings, separately from runtime representation.
//! Inferred container contents are discarded if the source can mutate or expose
//! them. An annotation remains trusted, just like an ordinary binding type.
use super::*;
use crate::adapter::TypeShape;

// Cyclic aliases can expand a small source into a large inferred shape.
const MAX_SHAPE_NODES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Shape {
    pub(super) ty: Ty,
    element: Option<Box<Shape>>,
    fields: BTreeMap<String, Shape>,
    nodes: usize,
    tuple_len: Option<usize>,
    rest: Option<Box<Shape>>,
    trusted: bool,
}

impl Shape {
    pub(super) fn new(ty: Ty) -> Self {
        Self {
            ty,
            element: None,
            fields: BTreeMap::new(),
            nodes: 1,
            tuple_len: None,
            rest: None,
            trusted: false,
        }
    }

    fn contents(ty: Ty, element: Option<Shape>, fields: BTreeMap<String, Shape>) -> Self {
        let nodes = 1
            + element.as_ref().map_or(0, |e| e.nodes)
            + fields.values().map(|f| f.nodes).sum::<usize>();
        if nodes > MAX_SHAPE_NODES {
            return Self::new(ty);
        }
        Self {
            ty,
            element: element.map(Box::new),
            fields,
            nodes,
            tuple_len: None,
            rest: None,
            trusted: false,
        }
    }

    fn trust(mut self) -> Self {
        self.trusted = true;
        self.element = self.element.map(|s| Box::new(s.trust()));
        self.fields = self
            .fields
            .into_iter()
            .map(|(k, s)| (k, s.trust()))
            .collect();
        self.rest = self.rest.map(|s| Box::new(s.trust()));
        self
    }

    pub(super) fn stored(self, stable: bool) -> Self {
        if !stable && !self.trusted {
            Self::new(self.ty)
        } else {
            self
        }
    }

    fn with_rest(mut self, rest: Option<Shape>) -> Self {
        self.nodes += rest.as_ref().map_or(0, |r| r.nodes);
        if self.nodes > MAX_SHAPE_NODES {
            return Self::new(self.ty);
        }
        self.rest = rest.map(Box::new);
        self
    }

    fn values(&self) -> Self {
        self.fields
            .values()
            .chain(self.rest.as_deref())
            .cloned()
            .reduce(|a, b| a.join(&b))
            .unwrap_or_else(|| Self::new(Ty::Unknown))
    }

    fn list(ty: Ty, element: Shape) -> Self {
        Self::contents(ty, Some(element), BTreeMap::new())
    }

    fn tuple(parts: Vec<Shape>, ty: Ty) -> Self {
        let len = parts.len();
        let element = parts.iter().fold(Self::new(Ty::Never), |a, b| a.join(b));
        let mut shape = Self::contents(
            ty,
            Some(element),
            parts
                .into_iter()
                .enumerate()
                .map(|(i, shape)| (i.to_string(), shape))
                .collect(),
        );
        if shape.element.is_some() {
            shape.tuple_len = Some(len);
        }
        shape
    }

    pub(super) fn join(&self, other: &Self) -> Self {
        if self.ty == Ty::Never {
            return other.clone();
        }
        if other.ty == Ty::Never {
            return self.clone();
        }
        let tuple_len = self.tuple_len.zip(other.tuple_len).map(|(a, b)| a.max(b));
        let fields = if let Some(len) = tuple_len {
            (0..len)
                .map(|index| {
                    let key = index.to_string();
                    (key.clone(), self.property(&key).join(&other.property(&key)))
                })
                .collect()
        } else {
            self.fields
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.join(other.fields.get(key)?))))
                .collect()
        };
        let mut shape = Self::contents(
            self.ty.join(&other.ty),
            self.element
                .as_ref()
                .zip(other.element.as_ref())
                .map(|(a, b)| a.join(b)),
            fields,
        );
        if shape.element.is_some() {
            shape.tuple_len = tuple_len;
        }
        shape.trusted = self.trusted && other.trusted;
        shape.with_rest(
            self.rest
                .as_deref()
                .zip(other.rest.as_deref())
                .map(|(a, b)| a.join(b)),
        )
    }

    pub(super) fn element(&self) -> Self {
        self.element.as_deref().cloned().unwrap_or_else(|| {
            Self::new(if self.ty == Ty::Text {
                Ty::Text
            } else {
                self.ty.element()
            })
        })
    }

    fn index(&self, index: usize) -> Self {
        if self.tuple_len.is_some_and(|len| index >= len) {
            Self::new(Ty::Undefined)
        } else {
            self.fields
                .get(&index.to_string())
                .cloned()
                .unwrap_or_else(|| self.element())
        }
    }

    fn property(&self, key: &str) -> Self {
        if matches!(self.ty, Ty::Array | Ty::List(_) | Ty::Text)
            && let Ok(index) = key.parse::<usize>()
            && index.to_string() == key
        {
            return self.index(index);
        }
        self.fields.get(key).cloned().unwrap_or_else(|| {
            self.rest
                .as_deref()
                .cloned()
                .unwrap_or_else(|| Self::new(self.ty.property(key)))
        })
    }
}

impl Facts {
    pub(super) fn annotation_shape(&self, annotation: &TypeAnnotation) -> Shape {
        use lash_kernel_value::Type;
        fn stated(ty: &Type) -> Shape {
            match ty {
                Type::Tuple(parts) => Shape::tuple(
                    parts.iter().map(stated).collect(),
                    Ty::List(Box::new(
                        parts
                            .iter()
                            .map(Ty::annotated)
                            .fold(Ty::Never, |a, b| a.join(&b)),
                    )),
                ),
                Type::List(element) => Shape::list(Ty::annotated(ty), stated(element)),
                Type::Set(element) => Shape::list(Ty::Set, stated(element)),
                Type::Map(map) => Shape::list(
                    Ty::Map,
                    Shape::tuple(vec![stated(&map.key), stated(&map.value)], Ty::Array),
                ),
                Type::Union(parts) => {
                    let mut shape = parts
                        .iter()
                        .map(stated)
                        .fold(Shape::new(Ty::Never), |a, b| a.join(&b));
                    shape.ty = Ty::annotated(ty);
                    shape
                }
                Type::Record(record) => Shape::contents(
                    Ty::annotated(ty),
                    None,
                    record
                        .fields
                        .iter()
                        .map(|f| {
                            let mut shape = stated(&f.ty);
                            if f.optional {
                                shape.ty = Ty::one_of(vec![shape.ty, Ty::Undefined]);
                            }
                            (f.name.clone(), shape)
                        })
                        .collect(),
                )
                .with_rest(record.rest.kept_type().map(stated)),
                _ => Shape::new(Ty::annotated(ty)),
            }
        }
        if let Ok(ty) = self.annotations.fact(annotation, false) {
            return stated(&ty);
        }
        match &annotation.shape {
            TypeShape::Tuple(parts) => Shape::tuple(
                parts.iter().map(|p| self.annotation_shape(p)).collect(),
                Ty::List(Box::new(
                    parts
                        .iter()
                        .map(|p| self.believed(p))
                        .fold(Ty::Never, |a, b| a.join(&b)),
                )),
            ),
            TypeShape::Array(element) => {
                Shape::list(self.believed(annotation), self.annotation_shape(element))
            }
            TypeShape::Generic(name, args) => match (name.as_str(), args.as_slice()) {
                ("Map" | "ReadonlyMap", [key, value]) => Shape::list(
                    Ty::Map,
                    Shape::tuple(
                        vec![self.annotation_shape(key), self.annotation_shape(value)],
                        Ty::Array,
                    ),
                ),
                ("Set" | "ReadonlySet", [element]) => {
                    Shape::list(Ty::Set, self.annotation_shape(element))
                }
                ("Array" | "ReadonlyArray", [element]) => {
                    Shape::list(self.believed(annotation), self.annotation_shape(element))
                }
                _ => Shape::new(self.believed(annotation)),
            },
            _ => Shape::new(self.believed(annotation)),
        }
    }

    pub(super) fn source_shape(&self, source: &Source<'_>, binding: usize) -> Shape {
        match source {
            Source::Annotated(annotation) => self.annotation_shape(annotation).trust(),
            Source::Value(value) => self.expression_shape(value),
            Source::ElementOf(value) => self.expression_shape(value).element(),
            Source::Index(source, index) => self.source_shape(source, binding).index(*index),
            Source::Property(source, key) => {
                if let Source::Value(namespace) = source.as_ref()
                    && let Some(path) = effect_path(namespace)
                    && path.split('.').next().is_some_and(|root| {
                        self.packages.is_root(root) && !self.named.contains(root)
                    })
                    && let Some(gives) = self.packages.call(&format!("{path}.{key}"))
                {
                    return Shape::new(Ty::Function(Box::new(gives.called(false))));
                }
                self.source_shape(source, binding).property(key)
            }
            Source::Default(source, value) => {
                let shape = self.source_shape(source, binding);
                if shape.ty == Ty::Undefined {
                    self.expression_shape(value)
                } else if shape.ty == Ty::Unknown {
                    shape
                } else if matches!(&shape.ty, Ty::Union(parts) if parts.contains(&Ty::Undefined)) {
                    let mut defined = shape;
                    defined.ty = defined.ty.without(&[Ty::Undefined]);
                    defined.join(&self.expression_shape(value))
                } else {
                    shape
                }
            }
            Source::ArrayRest(source, start) => {
                let shape = self.source_shape(source, binding);
                if let Some(len) = shape.tuple_len {
                    Shape::tuple(
                        (*start..len)
                            .map(|i| shape.property(&i.to_string()))
                            .collect(),
                        Ty::Array,
                    )
                } else {
                    Shape::list(Ty::Array, shape.element())
                }
            }
            Source::ObjectRest(source, keys) => {
                let shape = self.source_shape(source, binding);
                Shape::contents(
                    Ty::Object,
                    None,
                    shape
                        .fields
                        .into_iter()
                        .filter(|(key, _)| !keys.contains(key))
                        .collect(),
                )
                .with_rest(shape.rest.map(|r| *r))
            }
            Source::Opaque => Shape::new(Ty::Unknown),
            Source::Is(ty) => Shape::new(ty.clone()),
            Source::Step => Shape::new(unary_result(UnaryOp::Minus, &self.shapes[binding].ty)),
            Source::Returns(return_ty) => Shape::new(self.function(*return_ty)),
            Source::Compound(op, value) => Shape::new(binary_result(
                *op,
                &self.shapes[binding].ty,
                &self.type_of(value),
            )),
        }
    }

    pub(super) fn expression_shape(&self, expr: &Expr) -> Shape {
        match expr {
            Expr::Ident(name, _) if self.session.contains(&Name::new(name)) => {
                Shape::new(Ty::Unknown)
            }
            Expr::Ident(name, _) => self.binding(name).map_or_else(
                || Shape::new(self.type_of(expr)),
                |id| self.shapes[id].clone(),
            ),
            Expr::As { ty, .. } => self.annotation_shape(ty).trust(),
            Expr::Array(elements) => {
                let mut parts = Vec::new();
                let mut spread = None;
                for item in elements {
                    match item {
                        ArrayElement::Value(value) => parts.push(self.expression_shape(value)),
                        ArrayElement::Hole => parts.push(Shape::new(Ty::Undefined)),
                        ArrayElement::Spread(value) => {
                            let element = self.expression_shape(value).element();
                            spread = Some(
                                spread.map_or_else(|| element.clone(), |s: Shape| s.join(&element)),
                            );
                        }
                    }
                }
                let shape = Shape::tuple(parts, Ty::Array);
                spread.map_or_else(
                    || shape.clone(),
                    |spread| Shape::list(Ty::Array, shape.element().join(&spread)),
                )
            }
            Expr::Object(properties) => {
                let mut fields = BTreeMap::new();
                for property in properties {
                    match property {
                        ObjectProperty::KeyValue(PropertyKey::Static(key), value) => {
                            fields.insert(key.clone(), self.expression_shape(value));
                        }
                        _ => return Shape::new(Ty::Object),
                    }
                }
                Shape::contents(Ty::Object, None, fields)
            }
            Expr::New { constructor, args } => {
                if let Expr::Ident(name, _) = constructor.as_ref()
                    && self.unbound(name)
                    && let [CallArg::Value(value)] = args.as_slice()
                    && matches!(name.as_str(), "Map" | "Set")
                {
                    return Shape::list(
                        if name == "Map" { Ty::Map } else { Ty::Set },
                        self.expression_shape(value).element(),
                    );
                }
                Shape::new(self.type_of(expr))
            }
            Expr::Call { callee, args, .. } => {
                if let Expr::Member {
                    object,
                    property: MemberProperty::Field(method),
                    ..
                } = callee.as_ref()
                    && let Expr::Ident(root, _) = object.as_ref()
                    && root == "Object"
                    && self.unbound(root)
                {
                    if method == "keys" {
                        return Shape::list(Ty::Array, Shape::new(Ty::Text));
                    }
                    if method == "entries" {
                        let value = match args.as_slice() {
                            [CallArg::Value(value)] => self.expression_shape(value).values(),
                            _ => Shape::new(Ty::Unknown),
                        };
                        return Shape::list(
                            Ty::Array,
                            Shape::tuple(vec![Shape::new(Ty::Text), value], Ty::Array),
                        );
                    }
                }
                Shape::new(self.type_of(expr))
            }
            _ => Shape::new(self.type_of(expr)),
        }
    }
}
