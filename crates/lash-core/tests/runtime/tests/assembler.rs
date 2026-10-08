use super::*;

#[tokio::test]
async fn normalize_items_merges_adjacent_text_items() {
    let items = vec![
        InputItem::Text {
            text: "before ".to_string(),
        },
        InputItem::Text {
            text: "[file: host-prepared.txt]".to_string(),
        },
    ];
    let out = normalize_input_items(&items);
    assert_eq!(out.len(), 1);
    match &out[0] {
        NormalizedItem::Text(text) => {
            assert_eq!(text, "before [file: host-prepared.txt]");
        }
        _ => panic!("expected merged text item"),
    }
}
