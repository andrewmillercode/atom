use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
struct Foo {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

fn main() {
    let cases = [
        r#"{"id":"x","name":"y"}"#,
        r#"{"id":null,"name":null}"#,
        r#"{"id":"x","name":null}"#,
        r#"{}"#,
    ];
    for c in &cases {
        match serde_json::from_str::<Foo>(c) {
            Ok(v) => println!("OK   {c} -> id={:?} name={:?}", v.id, v.name),
            Err(e) => println!("FAIL {c} -> {e}"),
        }
    }
}
