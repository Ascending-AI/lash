use lash_render::{
    CutKind, Layout, RenderNode, RenderParams, RenderParamsPatch, RenderValue, Rendered,
    ShownRange, render, truncate_chars,
};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
struct CountedArray {
    value: Value,
    visits: Arc<AtomicUsize>,
}

impl RenderValue for CountedArray {
    fn node(&self) -> RenderNode<'_> {
        match &self.value {
            Value::Array(items) => RenderNode::Array(items.len()),
            Value::Number(number) => RenderNode::Number(Cow::Owned(number.to_string())),
            _ => unreachable!("the fixture contains only an array of numbers"),
        }
    }

    fn index(&self, index: usize) -> Option<Cow<'_, Self>> {
        let item = self.value.as_array()?.get(index)?.clone();
        self.visits.fetch_add(1, Ordering::SeqCst);
        Some(Cow::Owned(Self {
            value: item,
            visits: Arc::clone(&self.visits),
        }))
    }

    fn fields(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, Self>)> + '_ {
        std::iter::empty()
    }
}

#[test]
fn ax_ascii_corpus_matches_at_equivalent_parameters() {
    let fixture: Vec<Value> = serde_json::from_str(include_str!("ax/fixtures.json")).unwrap();
    for case in fixture {
        let max = case["max_chars"].as_u64().unwrap() as usize;
        let actual = truncate_chars(render(&case["input"], &RenderParams::ax(max)), max);
        assert_eq!(actual.body, case["body"], "{}", case["name"]);
    }
}

#[test]
fn array_threshold_and_item_floor_report_exact_paths() {
    let params = RenderParams {
        layout: Layout::Compact,
        max_chars: 60,
        min_item_chars: 8,
        ..RenderParams::default()
    };
    let ten = json!([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    let unchanged = render(&ten, &params);
    assert_eq!(unchanged.body, "[0,1,2,3,4,5,6,7,8,9]");
    assert_eq!(unchanged.cuts.original_chars, 21);
    assert!(unchanged.cuts.is_empty());

    let eleven = json!(["abcdefghijk", 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    let sampled = render(&eleven, &params);
    assert_eq!(sampled.body, "[\"abcdef...,1,2,... [6 hidden items],9,10]");
    assert_eq!(sampled.cuts.original_chars, 36);
    assert_eq!(sampled.cuts.counts[&CutKind::Array], 1);
    assert_eq!(sampled.cuts.counts[&CutKind::Item], 1);
    assert_eq!(
        sampled.cuts.shown,
        ["[0]", "[1]", "[2]", "[9]", "[10]"].map(|path| ShownRange::ValuePath(path.to_string()))
    );
}

#[test]
fn depth_and_stack_boundaries_report_original_characters() {
    let value = json!({"a": {"b": {"c": 1}}});
    let params = RenderParams {
        layout: Layout::Compact,
        max_depth: 2,
        ..RenderParams::default()
    };
    let rendered = render(&value, &params);
    assert_eq!(rendered.body, r#"{"a":{"b":"[Object]"}}"#);
    assert_eq!(rendered.cuts.original_chars, 19);
    assert_eq!(rendered.cuts.counts[&CutKind::Depth], 1);
    assert!(rendered.cuts.shown.is_empty());

    let stack = "Error\n    at a\n    at b\n    at c\n    at d\n    at e";
    let rendered = render(&json!({"stack": stack}), &RenderParams::default());
    assert_eq!(rendered.cuts.original_chars, stack.chars().count());
    assert_eq!(rendered.cuts.counts[&CutKind::Stack], 1);
    assert_eq!(
        rendered.body,
        "Error\n    at a\n    at b\n    at c\n    ... [1 frames hidden]\n    at e"
    );
    let params = RenderParams {
        stack_head: 4,
        stack_tail: 1,
        ..RenderParams::default()
    };
    assert!(render(&json!({"stack": stack}), &params).cuts.is_empty());
}

#[test]
fn empty_stack_falls_back_to_json() {
    let value = json!({"stack": "", "message": "boom"});
    let rendered = render(&value, &RenderParams::default());
    assert!(rendered.body.contains("\"message\": \"boom\""));
    assert!(rendered.body.contains("\"stack\": \"\""));
    assert_eq!(rendered.cuts.original_chars, rendered.body.chars().count());
}

#[test]
fn auto_inline_probe_stops_after_the_width_is_exceeded() {
    let visits = Arc::new(AtomicUsize::new(0));
    let value = CountedArray {
        value: Value::Array((0..1_000).map(|index| json!(index)).collect()),
        visits: Arc::clone(&visits),
    };
    let params = RenderParams {
        line_width: 8,
        array_threshold: 1_001,
        max_chars: 20_000,
        ..RenderParams::default()
    };
    let rendered = render(&value, &params);
    assert!(rendered.body.contains('\n'));
    assert!(visits.load(Ordering::SeqCst) < 2_100);
}

#[test]
fn final_cut_uses_unicode_scalars_and_reports_the_shown_range() {
    let rendered = truncate_chars(render(&json!("é🙂z"), &RenderParams::default()), 2);
    assert_eq!(rendered.body, "é🙂\n...[truncated 1 chars]");
    assert_eq!(rendered.cuts.original_chars, 3);
    assert_eq!(rendered.cuts.counts[&CutKind::Chars], 1);
    assert_eq!(
        rendered.cuts.shown,
        vec![ShownRange::Text {
            block: 0,
            chars: 0..2,
            lines: 0..1,
        }]
    );
}

#[test]
fn layouts_and_width_and_indent_set_the_frame() {
    let value = json!({"a": [1, 2]});
    let base = RenderParams::default();
    assert_eq!(render(&value, &base).body, r#"{"a": [1, 2]}"#);
    let narrow = RenderParams {
        line_width: 10,
        ..base.clone()
    };
    assert_eq!(
        render(&value, &narrow).body,
        "{\n  \"a\": [\n    1,\n    2\n  ]\n}"
    );
    let wide_indent = RenderParams {
        indent: 4,
        layout: Layout::Pretty,
        ..base.clone()
    };
    assert_eq!(
        render(&value, &wide_indent).body,
        "{\n    \"a\": [\n        1,\n        2\n    ]\n}"
    );
    let compact = RenderParams {
        layout: Layout::Compact,
        ..base
    };
    assert_eq!(render(&value, &compact).body, r#"{"a":[1,2]}"#);
    let zero_indent = RenderParams {
        layout: Layout::Pretty,
        indent: 0,
        ..RenderParams::default()
    };
    assert_eq!(render(&value, &zero_indent).body, r#"{"a":[1,2]}"#);
}

#[test]
fn auto_breaks_at_the_default_eighty_column_boundary() {
    let params = RenderParams {
        array_threshold: usize::MAX,
        ..RenderParams::default()
    };
    let mut eighty = vec![1; 26];
    eighty[24] = 10;
    eighty[25] = 10;
    let fitted = render(&json!(eighty), &params);
    assert_eq!(fitted.cuts.original_chars, 80);
    assert_eq!(fitted.body.chars().count(), 80);
    assert!(!fitted.body.contains('\n'));
    assert!(fitted.cuts.is_empty());

    let broken = render(&json!(vec![1; 27]), &params);
    assert_eq!(broken.cuts.original_chars, 137);
    assert_eq!(broken.body.chars().count(), 137);
    assert!(broken.body.starts_with("[\n  1,\n  1"));
    assert!(broken.body.ends_with("\n]"));
    assert!(broken.cuts.is_empty());
}

#[test]
fn uncut_count_matches_escaped_output_without_sampling_nested_arrays() {
    let escaped = json!({"q\n": "\u{0000}\u{001f}\u{0008}\t\n\u{000c}\r\"\\é"});
    let params = RenderParams {
        layout: Layout::Compact,
        ..RenderParams::default()
    };
    let rendered = render(&escaped, &params);
    assert_eq!(rendered.body, escaped.to_string());
    assert_eq!(rendered.cuts.original_chars, rendered.body.chars().count());
    assert!(rendered.cuts.is_empty());

    let nested = json!({"items": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]});
    let rendered = render(&nested, &params);
    assert_eq!(rendered.body, nested.to_string());
    assert_eq!(rendered.cuts.original_chars, rendered.body.chars().count());
    assert!(rendered.cuts.is_empty());
}

#[test]
fn sparse_patches_layer_every_parameter() {
    let host = RenderParamsPatch {
        max_chars: Some(12),
        layout: Some(Layout::Pretty),
        line_width: Some(20),
        indent: Some(4),
        max_depth: Some(2),
        array_threshold: Some(5),
        array_head: Some(2),
        array_tail: Some(1),
        min_item_chars: Some(7),
        stack_head: Some(2),
        stack_tail: Some(2),
    };
    let turn = RenderParamsPatch {
        max_chars: Some(6),
        array_head: Some(1),
        ..RenderParamsPatch::default()
    };
    let resolved = turn.over(&host).apply(&RenderParams::default());
    assert_eq!(resolved.max_chars, 6);
    assert_eq!(resolved.array_head, 1);
    assert_eq!(resolved.stack_tail, 2);
    assert_eq!(resolved.indent, 4);
    assert_eq!(resolved.max_depth, 2);
    assert_eq!(resolved.array_threshold, 5);
    assert_eq!(resolved.array_tail, 1);
    assert_eq!(resolved.min_item_chars, 7);
    assert_eq!(resolved.stack_head, 2);
    assert_eq!(resolved.line_width, 20);
    assert_eq!(resolved.layout, Layout::Pretty);
    let encoded = serde_json::to_value(&turn).unwrap();
    assert_eq!(encoded, json!({"max_chars": 6, "array_head": 1}));
    assert_eq!(
        serde_json::from_value::<RenderParamsPatch>(encoded).unwrap(),
        turn
    );
    assert!(serde_json::from_value::<RenderParamsPatch>(json!({"extra": 1})).is_err());
    assert!(RenderParamsPatch::default().is_empty());
    assert_eq!(RenderParams::preview().max_depth, 2);
    assert_eq!(RenderParams::preview().max_chars, 1_000);
    assert_eq!(RenderParams::preview().layout, Layout::Compact);
}

#[test]
fn cut_report_merge_preserves_counts_and_ranges() {
    let mut a: Rendered<String> = render(&json!(vec![0; 11]), &RenderParams::default());
    let b = truncate_chars(render(&json!("é🙂"), &RenderParams::default()), 1);
    let original = a.cuts.original_chars;
    a.cuts.merge(b.cuts);
    assert_eq!(a.cuts.original_chars, original + 2);
    assert_eq!(a.cuts.counts[&CutKind::Array], 1);
    assert_eq!(a.cuts.counts[&CutKind::Chars], 1);
    assert_eq!(a.cuts.shown.len(), 6);
}

#[test]
fn sampling_parameters_change_item_budget_and_selected_indices() {
    let value = json!(["abcdefghij", "middle", "uvwxyzabcd"]);
    let params = RenderParams {
        layout: Layout::Compact,
        array_threshold: 2,
        array_head: 1,
        array_tail: 1,
        max_chars: 18,
        min_item_chars: 5,
        ..RenderParams::default()
    };
    let short = render(&value, &params);
    assert_eq!(short.body, "[\"ab...,... [1 hidden items],\"uv...]");
    assert_eq!(short.cuts.original_chars, 36);
    assert_eq!(short.cuts.counts[&CutKind::Array], 1);
    assert_eq!(short.cuts.counts[&CutKind::Item], 2);
    assert_eq!(
        short.cuts.shown,
        vec![
            ShownRange::ValuePath("[0]".into()),
            ShownRange::ValuePath("[2]".into())
        ]
    );

    let floor = RenderParams {
        min_item_chars: 12,
        ..params.clone()
    };
    let long = render(&value, &floor);
    assert_eq!(
        long.body,
        "[\"abcdefghij\",... [1 hidden items],\"uvwxyzabcd\"]"
    );
    assert!(!long.cuts.counts.contains_key(&CutKind::Item));

    let wider = RenderParams {
        max_chars: 60,
        min_item_chars: 5,
        array_head: 2,
        ..params
    };
    let selected = render(&value, &wider);
    assert_eq!(selected.cuts.shown.len(), 3);
    assert!(!selected.cuts.counts.contains_key(&CutKind::Item));
}

#[derive(Clone)]
enum SpecialValue {
    Undefined,
    Placeholder,
    Object,
    Array,
}

impl RenderValue for SpecialValue {
    fn node(&self) -> RenderNode<'_> {
        match self {
            Self::Undefined => RenderNode::Undefined,
            Self::Placeholder => RenderNode::Placeholder(Cow::Borrowed("<handle>")),
            Self::Object => RenderNode::Object(2),
            Self::Array => RenderNode::Array(2),
        }
    }

    fn index(&self, index: usize) -> Option<Cow<'_, Self>> {
        matches!(self, Self::Array)
            .then(|| match index {
                0 => Self::Undefined,
                _ => Self::Placeholder,
            })
            .map(Cow::Owned)
    }

    fn fields(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, Self>)> + '_ {
        let fields = if matches!(self, Self::Object) {
            vec![
                (Cow::Borrowed("skip"), Cow::Owned(Self::Undefined)),
                (Cow::Borrowed("keep"), Cow::Owned(Self::Placeholder)),
            ]
        } else {
            Vec::new()
        };
        fields.into_iter()
    }
}

#[test]
fn undefined_and_placeholders_follow_the_descriptor_contract() {
    let compact = RenderParams {
        layout: Layout::Compact,
        ..RenderParams::default()
    };
    assert_eq!(render(&SpecialValue::Undefined, &compact).body, "undefined");
    assert_eq!(
        render(&SpecialValue::Object, &compact).body,
        "{\"keep\":<handle>}"
    );
    assert_eq!(
        render(&SpecialValue::Array, &compact).body,
        "[null,<handle>]"
    );
}
