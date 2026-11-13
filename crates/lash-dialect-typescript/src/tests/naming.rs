//! Laws of operation names in TypeScript cells: OP-NAME,
//! OP-SPELLING-REPAIR, NS-ESCAPE and PRINT-ROUNDTRIP.

use std::collections::BTreeMap;

use {
    lash_kernel_value::EffectName, lash_kernel_value::Name, lash_kernel_value::Param,
    lash_kernel_value::Signature, lash_kernel_value::Type,
};

use super::{lower_against, print_document};

/// Each of `names` as an operation taking one `input` and giving anything.
fn effects(names: &[&str]) -> BTreeMap<EffectName, Signature> {
    names
        .iter()
        .map(|name| {
            (
                EffectName::new(*name).expect("an effect name"),
                Signature {
                    params: vec![Param {
                        name: Name::new("input"),
                        ty: Type::Any,
                        optional: false,
                    }],
                    result: Type::Any,
                },
            )
        })
        .collect()
}

/// OP-SPELLING-REPAIR: the canonical spelling of a multiword operation is
/// another dialect's, refused with the camelCase repair.
#[test]
fn the_canonical_spelling_of_a_multiword_operation_is_refused_with_its_repair() {
    let error = lower_against(
        "await inbox.send_item({});",
        &[],
        &effects(&["inbox.send_item"]),
    )
    .expect_err("TypeScript spells `send_item` as `sendItem`");
    assert_eq!(error.code().as_str(), "TS_OPERATION_SPELLING", "{error}");
    assert!(
        error
            .suggestions
            .iter()
            .any(|repair| repair == "write `inbox.sendItem`"),
        "{error:?}"
    );
}

/// NS-ESCAPE: an operation whose spelling is a keyword stays callable
/// through the qualified escape.
#[test]
fn a_keyword_operation_is_callable_through_the_qualified_escape() {
    let lowered = lower_against(
        "await records[\"delete\"]({ id: 1 });",
        &[],
        &effects(&["records.delete"]),
    )
    .expect("the escape names the operation");
    let text = print_document(&lowered.document);
    assert!(
        text.contains(&format!(
            "perform {}(",
            super::linked_inspection::operation_slot(&lowered.document, "records.delete")
                .expect("operation import")
                .0
        )),
        "{text}"
    );
}

/// FIG-7711: every admitted statement and expression form has a faithful
/// source spelling in a cell and in module main. Refused inputs are listed
/// separately and never counted as successful round trips.
#[test]
fn enumerated_statements_and_expressions_print_and_relower() {
    use lash_kernel_dialect::{Environment, Mode};
    let library = super::library();
    let namespaces = Default::default();
    let environment = Environment::new(library, &namespaces);
    let forms = flat_source_forms();
    let mut refused = Vec::new();
    let mut failures = Vec::new();
    let mut admitted = 0;
    for (name, body) in forms {
        for module in [false, true] {
            let context = if module { "module" } else { "cell" };
            let source = if module {
                format!("export const entry=async(): Promise<number>=>1; {body}")
            } else {
                body.into()
            };
            let mode = if module {
                Mode::Module
            } else {
                Mode::Cell(lash_kernel_dialect::CellEnvironment::first())
            };
            let lowered = match crate::lower(&source, &environment, mode) {
                Ok(lowered) => lowered,
                Err(error) => {
                    refused.push(format!("{context} {name}: {error}"));
                    continue;
                }
            };
            admitted += 1;
            let printed = match crate::print(
                &lowered.document,
                Some(&lowered.captions),
                library,
                lash_kernel_dialect::PrintOptions {
                    source_map: true,
                    ..Default::default()
                },
            ) {
                Ok(printed) => printed,
                Err(error) => {
                    failures.push(format!("{context} {name}: print: {error}"));
                    continue;
                }
            };
            match crate::lower(&printed.source, &environment, mode) {
                Ok(read)
                    if read.document.canonical_bytes() == lowered.document.canonical_bytes() =>
                {
                    printed
                        .source_map
                        .unwrap()
                        .validate(&lowered.document, &printed.source)
                        .unwrap();
                }
                Ok(read) => failures.push(format!(
                    "{context} {name}: different document; private {:?} -> {:?}; source {}",
                    lowered.document.private_bindings,
                    read.document.private_bindings,
                    printed.source
                )),
                Err(error) => failures.push(format!("{context} {name}: lower print: {error}")),
            }
        }
    }
    eprintln!(
        "TypeScript enumerated: admitted={admitted}, passed={}, failed={}, refused={}\nREFUSED:\n{}",
        admitted - failures.len(),
        failures.len(),
        refused.len(),
        refused.join("\n")
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(admitted, 448, "admitted forms must not disappear");
    let mut refused_forms = refused
        .iter()
        .map(|failure| failure.split(':').next().unwrap())
        .collect::<Vec<_>>();
    refused_forms.sort_unstable();
    let mut expected = [
        "return",
        "return bare",
        "break",
        "continue",
        "class",
        "for in assignment array",
        "for in assignment object",
        "for await",
    ]
    .into_iter()
    .flat_map(|name| [format!("cell {name}"), format!("module {name}")])
    .collect::<Vec<_>>();
    expected.sort_unstable();
    assert_eq!(
        refused_forms, expected,
        "lower-refused forms must be explicit"
    );
}

fn flat_source_forms() -> Vec<(&'static str, &'static str)> {
    vec![
        ("empty", "; debugger;"),
        (
            "authored suffix bindings",
            "let x=1;let x_1=2;{let x=3;let x_1=4;const result=x+x_1;}",
        ),
        (
            "authored suffix function",
            "function f_1(x_1:number):number{return x_1+1;}const result=f_1(1);",
        ),
        ("block", "{let x=1; x+=2;}"),
        ("nested shadow", "let x=1; {let x=2; {const x=3;}}"),
        (
            "type declarations",
            "type T=number; interface Shape {x:number} const x:T=1;",
        ),
        ("enum", "enum E {A,B=3,C=\"c\"} const result=E.A;"),
        ("declaration without initializer", "let x; var y;"),
        ("multiple declarations", "let x=1,y=2; const z=x+y;"),
        ("if", "let result=0; if(true){result=1;}"),
        (
            "if else",
            "let result=0; if(true){result=1;}else{result=2;}",
        ),
        (
            "if else if",
            "let result=0; if(false){result=1;}else if(true){result=2;}else{result=3;}",
        ),
        ("while", "let result=0; while(result<2){result++;}"),
        (
            "while break continue",
            "let result=0; while(true){result++;if(result===1)continue;break;}",
        ),
        ("do while", "let result=0; do{result++;}while(result<2);"),
        (
            "do while exits",
            "let result=0; do{result++;if(result===1)continue;break;}while(true);",
        ),
        ("for", "let result=0; for(let i=0;i<2;i++){result+=i;}"),
        ("for destructured initializer", "for(let [x]=[0];x<2;x++){}"),
        (
            "switch",
            "let result=0;switch(2){case 1:result=1;break;case 2:result=2;break;default:result=3;}",
        ),
        (
            "switch fallthrough",
            "let result=0;switch(1){case 1:result++;case 2:result+=2;default:result+=3;}",
        ),
        (
            "switch default first",
            "let result=0;switch(2){default:result++;case 2:result+=2;break;case 3:result+=3;}",
        ),
        (
            "switch no default",
            "let result=0;switch(2){case 1:result=1;case 2:result=2;break;}",
        ),
        ("switch empty", "switch(2){}"),
        (
            "switch continue",
            "let result=0;for(let i=0;i<3;i++){switch(i){case 1:continue;default:result+=i;}}",
        ),
        ("throw", "throw 1;"),
        ("return", "return 1;"),
        ("return bare", "return;"),
        ("break", "break;"),
        ("continue", "continue;"),
        (
            "return in function",
            "const f=():number=>{if(true)return 1;return 2;};",
        ),
        ("bare return in function", "const f=():void=>{return;};"),
        (
            "named function",
            "function local(x:number):number{return x+1;} const result=local(2);",
        ),
        (
            "async function",
            "async function local(x:number):Promise<number>{return x+1;} const result=await local(2);",
        ),
        ("arrow", "const f=(x:number):number=>x+1;const result=f(2);"),
        (
            "function expression",
            "const f=function named(x:number):number{return x+1;};const result=f(2);",
        ),
        (
            "function arguments",
            "const f=function():number{return arguments.length;};",
        ),
        ("await", "const result=await Promise.resolve(2);"),
        ("class", "class Box{x=1;}"),
        (
            "label block",
            "let result=0;outer:{result=1;break outer;result=2;}",
        ),
        (
            "label nested",
            "let result=0;outer:for(const x of [1,2]){for(const y of [3,4]){result+=x+y;if(y===3)continue outer;break outer;}}",
        ),
        (
            "label catch finally",
            "outer:while(true){try{break outer;}catch{}finally{let done=1;}}",
        ),
        (
            "invented name collision",
            "let t1=0;try{throw 1;}catch{t1=2;}",
        ),
        (
            "source numeric suffix",
            "let result=0;try{throw 1;}catch(error_1){result=error_1;}",
        ),
        ("var name", "var x=1;"),
        ("var array", "var [x,y]=[1,2];"),
        ("var object", "var {a:x,b:y}={a:1,b:2};"),
        ("var array defaults", "var [x=1,,...rest]=[undefined,2,3];"),
        (
            "var object defaults",
            "var {a:x=1,...rest}={a:undefined,b:2};",
        ),
        ("var nested", "var {a:[x,{b:y}]}={a:[1,{b:2}]};"),
        ("let name", "let x=1;"),
        ("let array", "let [x,y]=[1,2];"),
        ("let object", "let {a:x,b:y}={a:1,b:2};"),
        ("let array defaults", "let [x=1,,...rest]=[undefined,2,3];"),
        (
            "let object defaults",
            "let {a:x=1,...rest}={a:undefined,b:2};",
        ),
        ("let nested", "let {a:[x,{b:y}]}={a:[1,{b:2}]};"),
        ("const name", "const x=1;"),
        ("const array", "const [x,y]=[1,2];"),
        ("const object", "const {a:x,b:y}={a:1,b:2};"),
        (
            "const array defaults",
            "const [x=1,,...rest]=[undefined,2,3];",
        ),
        (
            "const object defaults",
            "const {a:x=1,...rest}={a:undefined,b:2};",
        ),
        ("const nested", "const {a:[x,{b:y}]}={a:[1,{b:2}]};"),
        ("assignment variable =", "let x:any=2; x=1;"),
        ("assignment field =", "const o={x:2}; o.x=1;"),
        ("assignment index =", "const a=[2]; a[0]=1;"),
        ("assignment variable +=", "let x:any=2; x+=1;"),
        ("assignment field +=", "const o={x:2}; o.x+=1;"),
        ("assignment index +=", "const a=[2]; a[0]+=1;"),
        ("assignment variable -=", "let x:any=2; x-=1;"),
        ("assignment field -=", "const o={x:2}; o.x-=1;"),
        ("assignment index -=", "const a=[2]; a[0]-=1;"),
        ("assignment variable *=", "let x:any=2; x*=1;"),
        ("assignment field *=", "const o={x:2}; o.x*=1;"),
        ("assignment index *=", "const a=[2]; a[0]*=1;"),
        ("assignment variable /=", "let x:any=2; x/=1;"),
        ("assignment field /=", "const o={x:2}; o.x/=1;"),
        ("assignment index /=", "const a=[2]; a[0]/=1;"),
        ("assignment variable %=", "let x:any=2; x%=1;"),
        ("assignment field %=", "const o={x:2}; o.x%=1;"),
        ("assignment index %=", "const a=[2]; a[0]%=1;"),
        ("assignment variable **=", "let x:any=2; x**=1;"),
        ("assignment field **=", "const o={x:2}; o.x**=1;"),
        ("assignment index **=", "const a=[2]; a[0]**=1;"),
        ("assignment variable <<=", "let x:any=2; x<<=1;"),
        ("assignment field <<=", "const o={x:2}; o.x<<=1;"),
        ("assignment index <<=", "const a=[2]; a[0]<<=1;"),
        ("assignment variable >>=", "let x:any=2; x>>=1;"),
        ("assignment field >>=", "const o={x:2}; o.x>>=1;"),
        ("assignment index >>=", "const a=[2]; a[0]>>=1;"),
        ("assignment variable >>>=", "let x:any=2; x>>>=1;"),
        ("assignment field >>>=", "const o={x:2}; o.x>>>=1;"),
        ("assignment index >>>=", "const a=[2]; a[0]>>>=1;"),
        ("assignment variable &=", "let x:any=2; x&=1;"),
        ("assignment field &=", "const o={x:2}; o.x&=1;"),
        ("assignment index &=", "const a=[2]; a[0]&=1;"),
        ("assignment variable |=", "let x:any=2; x|=1;"),
        ("assignment field |=", "const o={x:2}; o.x|=1;"),
        ("assignment index |=", "const a=[2]; a[0]|=1;"),
        ("assignment variable ^=", "let x:any=2; x^=1;"),
        ("assignment field ^=", "const o={x:2}; o.x^=1;"),
        ("assignment index ^=", "const a=[2]; a[0]^=1;"),
        ("assignment variable &&=", "let x:any=2; x&&=1;"),
        ("assignment field &&=", "const o={x:2}; o.x&&=1;"),
        ("assignment index &&=", "const a=[2]; a[0]&&=1;"),
        ("assignment variable ||=", "let x:any=2; x||=1;"),
        ("assignment field ||=", "const o={x:2}; o.x||=1;"),
        ("assignment index ||=", "const a=[2]; a[0]||=1;"),
        ("assignment variable ??=", "let x:any=2; x??=1;"),
        ("assignment field ??=", "const o={x:2}; o.x??=1;"),
        ("assignment index ??=", "const a=[2]; a[0]??=1;"),
        ("for parts False False False", "let i=0;for(;;){break;}"),
        ("for parts False False True", "let i=0;for(;;i++){break;}"),
        ("for parts False True False", "let i=0;for(;i<2;){break;}"),
        ("for parts False True True", "let i=0;for(;i<2;i++){break;}"),
        ("for parts True False False", "for(let i=0;;){break;}"),
        ("for parts True False True", "for(let i=0;;i++){break;}"),
        ("for parts True True False", "for(let i=0;i<2;){break;}"),
        ("for parts True True True", "for(let i=0;i<2;i++){break;}"),
        ("for of const name", "for(const x of [[1,2]]){}"),
        ("for of let name", "for(let x of [[1,2]]){}"),
        ("for of var name", "for(var x of [[1,2]]){}"),
        (
            "for of assignment name",
            "let x:any;let y:any;for( x of [[1,2]]){}",
        ),
        ("for of const array", "for(const [x,y] of [[1,2]]){}"),
        ("for of let array", "for(let [x,y] of [[1,2]]){}"),
        ("for of var array", "for(var [x,y] of [[1,2]]){}"),
        (
            "for of assignment array",
            "let x:any;let y:any;for( [x,y] of [[1,2]]){}",
        ),
        ("for of const object", "for(const {a:x} of [{a:1}]){}"),
        ("for of let object", "for(let {a:x} of [{a:1}]){}"),
        ("for of var object", "for(var {a:x} of [{a:1}]){}"),
        (
            "for of assignment object",
            "let x:any;let y:any;for( {a:x} of [{a:1}]){}",
        ),
        ("for in const name", "for(const x in {a:1,b:2}){}"),
        ("for in let name", "for(let x in {a:1,b:2}){}"),
        ("for in var name", "for(var x in {a:1,b:2}){}"),
        (
            "for in assignment name",
            "let x:any;let y:any;for( x in {a:1,b:2}){}",
        ),
        ("for in const array", "for(const [x,y] in {a:1,b:2}){}"),
        ("for in let array", "for(let [x,y] in {a:1,b:2}){}"),
        ("for in var array", "for(var [x,y] in {a:1,b:2}){}"),
        (
            "for in assignment array",
            "let x:any;let y:any;for( [x,y] in {a:1,b:2}){}",
        ),
        ("for in const object", "for(const {a:x} in {a:1,b:2}){}"),
        ("for in let object", "for(let {a:x} in {a:1,b:2}){}"),
        ("for in var object", "for(var {a:x} in {a:1,b:2}){}"),
        (
            "for in assignment object",
            "let x:any;let y:any;for( {a:x} in {a:1,b:2}){}",
        ),
        ("label while", "outer:while(true){break outer;}"),
        ("label do", "outer:do{break outer;}while(true);"),
        ("label for", "outer:for(;;){break outer;}"),
        ("label for of", "outer:for(const x of [1]){break outer;}"),
        ("label for in", "outer:for(const x in {a:1}){break outer;}"),
        ("label switch", "outer:switch(1){case 1:break outer;}"),
        (
            "try catch none finally=False",
            "let result=0;try{throw 1;}catch{result=2;}",
        ),
        (
            "try catch none finally=True",
            "let result=0;try{throw 1;}catch{result=2;}finally{result+=1;}",
        ),
        (
            "try catch (error) finally=False",
            "let result=0;try{throw 1;}catch(error){result=2;}",
        ),
        (
            "try catch (error) finally=True",
            "let result=0;try{throw 1;}catch(error){result=2;}finally{result+=1;}",
        ),
        (
            "try catch ([error]) finally=False",
            "let result=0;try{throw 1;}catch([error]){result=2;}",
        ),
        (
            "try catch ([error]) finally=True",
            "let result=0;try{throw 1;}catch([error]){result=2;}finally{result+=1;}",
        ),
        (
            "try catch ({message:error}) finally=False",
            "let result=0;try{throw 1;}catch({message:error}){result=2;}",
        ),
        (
            "try catch ({message:error}) finally=True",
            "let result=0;try{throw 1;}catch({message:error}){result=2;}finally{result+=1;}",
        ),
        (
            "try catch ({message:error=\"fallback\",...rest}) finally=False",
            "let result=0;try{throw 1;}catch({message:error=\"fallback\",...rest}){result=2;}",
        ),
        (
            "try catch ({message:error=\"fallback\",...rest}) finally=True",
            "let result=0;try{throw 1;}catch({message:error=\"fallback\",...rest}){result=2;}finally{result+=1;}",
        ),
        (
            "try finally",
            "let result=0;try{result=1;}finally{result=2;}",
        ),
        ("try empty finally", "try{}finally{}"),
        (
            "expression null",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(null);",
        ),
        (
            "expression bool",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(true);",
        ),
        (
            "expression number",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(1.5);",
        ),
        (
            "expression bigint",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(2n);",
        ),
        (
            "expression string",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(\"text\");",
        ),
        (
            "expression regexp",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(/a+/gi);",
        ),
        (
            "expression ident",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(x);",
        ),
        (
            "expression this",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(this);",
        ),
        (
            "expression sequence",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=((x,y));",
        ),
        (
            "expression array",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=([1,,...xs]);",
        ),
        (
            "expression object",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=({a:x,[y]:2,...obj});",
        ),
        (
            "expression member",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(obj.a);",
        ),
        (
            "expression index",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(xs[x]);",
        ),
        (
            "expression conditional",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(x?obj.a:y);",
        ),
        (
            "expression template",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(`hello ${x}`);",
        ),
        (
            "expression tagged template",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(tag`hello ${x}`);",
        ),
        (
            "expression function",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(function(a:any):any{return a;});",
        ),
        (
            "expression arrow",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=((a:any):any=>a);",
        ),
        (
            "expression call",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(f(x));",
        ),
        (
            "expression spread call",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(f(...xs));",
        ),
        (
            "expression method",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(obj.f(x));",
        ),
        (
            "expression spread method",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(obj.f(...xs));",
        ),
        (
            "expression new",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(new Array(x));",
        ),
        (
            "expression dynamic new",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(new f(x));",
        ),
        (
            "expression optional field",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(obj?.a);",
        ),
        (
            "expression optional index",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(xs?.[x]);",
        ),
        (
            "expression optional call",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(f?.(x));",
        ),
        (
            "expression optional method",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(obj?.f?.(x));",
        ),
        (
            "expression optional delete",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(delete obj?.a);",
        ),
        (
            "expression await",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(await Promise.resolve(x));",
        ),
        (
            "expression post increment",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(x++);",
        ),
        (
            "expression pre increment",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(++x);",
        ),
        (
            "expression post decrement",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(x--);",
        ),
        (
            "expression pre decrement",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(--x);",
        ),
        (
            "expression delete",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(delete obj.a);",
        ),
        (
            "expression delete index",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(delete xs[x]);",
        ),
        (
            "expression as",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(x as number);",
        ),
        (
            "expression logical and",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(x&&y);",
        ),
        (
            "expression logical or",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(x||y);",
        ),
        (
            "expression nullish",
            "let x:any=1;let y:any=2;const xs=[1,2];const obj={a:1,f:(z:any)=>z};const f=(z:any):any=>z;const tag=(s:any,z:any):any=>z;const result=(x??y);",
        ),
        ("binary +", "let x:any=1;let y:any=2;const result=x + y;"),
        ("binary -", "let x:any=1;let y:any=2;const result=x - y;"),
        ("binary *", "let x:any=1;let y:any=2;const result=x * y;"),
        ("binary /", "let x:any=1;let y:any=2;const result=x / y;"),
        ("binary %", "let x:any=1;let y:any=2;const result=x % y;"),
        ("binary **", "let x:any=1;let y:any=2;const result=x ** y;"),
        ("binary <", "let x:any=1;let y:any=2;const result=x < y;"),
        ("binary <=", "let x:any=1;let y:any=2;const result=x <= y;"),
        ("binary >", "let x:any=1;let y:any=2;const result=x > y;"),
        ("binary >=", "let x:any=1;let y:any=2;const result=x >= y;"),
        (
            "binary ===",
            "let x:any=1;let y:any=2;const result=x === y;",
        ),
        (
            "binary !==",
            "let x:any=1;let y:any=2;const result=x !== y;",
        ),
        ("binary ==", "let x:any=1;let y:any=2;const result=x == y;"),
        ("binary !=", "let x:any=1;let y:any=2;const result=x != y;"),
        ("binary &", "let x:any=1;let y:any=2;const result=x & y;"),
        ("binary |", "let x:any=1;let y:any=2;const result=x | y;"),
        ("binary ^", "let x:any=1;let y:any=2;const result=x ^ y;"),
        ("binary <<", "let x:any=1;let y:any=2;const result=x << y;"),
        ("binary >>", "let x:any=1;let y:any=2;const result=x >> y;"),
        (
            "binary >>>",
            "let x:any=1;let y:any=2;const result=x >>> y;",
        ),
        ("binary in", "let x:any=1;let y:any=2;const result=x in y;"),
        (
            "binary instanceof",
            "let x:any=1;let y:any=2;const result=x instanceof y;",
        ),
        ("unary +", "let x:any=1;const result=+x;"),
        ("unary -", "let x:any=1;const result=-x;"),
        ("unary !", "let x:any=1;const result=!x;"),
        ("unary ~", "let x:any=1;const result=~x;"),
        ("unary typeof", "let x:any=1;const result=typeof x;"),
        ("unary void", "let x:any=1;const result=void x;"),
        ("enum string", "enum Status {Ready='ready'};"),
        ("enum empty", "enum Empty {}"),
        (
            "async function expression",
            "const f=async function named(x:number):Promise<number>{return x+1;};",
        ),
        (
            "function this",
            "const f=function named():unknown{return this;};",
        ),
        ("for await", "for await(const x of [1,2]){}"),
    ]
}

fn review_source_forms() -> Vec<(String, String)> {
    vec![
        ("review ts-230 nested switch try loop".into(), "let result=0;try{switch(1){case 1: for(const [x,y] of [[1,2]]){result+=x+y;} break;default:result=4;}}catch{result=9;}finally{result+=1;}".into()),
        ("review ts-231 label continue switch".into(), "let result=0;outer:for(let i=0;i<3;i++){switch(i){case 1:continue outer;default:result+=i;}}".into()),
        ("review ts-232 switch continue step two".into(), "let result=0;for(let i=0;i<4;i+=2){switch(i){case 0:continue;default:result+=i;}}".into()),
        ("review ts-233 switch continue custom step".into(), "let result=0;for(let i=0;i<4;i=i+2){switch(i){case 0:continue;default:result+=i;}}".into()),
        ("review ts-234 nested switch continue".into(), "let result=0;for(let i=0;i<3;i++){switch(i){case 0:switch(i){case 0:continue;}break;default:result+=i;}}".into()),
        ("review ts-235 catch shadow".into(), "let e=1;let result=0;try{throw 2;}catch(e){let f=()=>e;result=f();}result+=e;".into()),
        ("review ts-236 catch nested shadow".into(), "let result=0;try{throw {a:1};}catch({a}){try{throw 2;}catch(a){result=a;}}".into()),
        ("review ts-237 capture classic let".into(), "const fs=[];for(let i=0;i<3;i++){fs.push(()=>i);}const result=fs.map(f=>f());".into()),
        ("review ts-238 capture for of".into(), "const fs=[];for(const [x] of [[1],[2]]){fs.push(()=>x);}const result=fs.map(f=>f());".into()),
        ("review ts-239 capture var".into(), "const fs=[];for(var i=0;i<3;i++){fs.push(()=>i);}const result=fs.map(f=>f());".into()),
        ("review ts-240 await switch".into(), "let result=0;switch(await Promise.resolve(1)){case await Promise.resolve(1):result=await Promise.resolve(2);break;}".into()),
        ("review ts-241 await loop".into(), "let result=0;for(let i=await Promise.resolve(0);await Promise.resolve(i<2);i+=await Promise.resolve(1)){result+=await Promise.resolve(i);}".into()),
        ("review ts-242 await patterns".into(), "let result=0;for(const {x=await Promise.resolve(3)} of await Promise.resolve([{}])){result=x;}".into()),
        ("review ts-243 await try".into(), "let result=0;try{throw await Promise.resolve(1);}catch(e){result=await Promise.resolve(e);}finally{result+=await Promise.resolve(2);}".into()),
        ("review ts-244 destructure while label".into(), "let result=0;outer:while(result<2){const [x]=[1];result+=x;continue outer;}".into()),
        ("review ts-245 destructure do label".into(), "let result=0;outer:do{const {x}={x:1};result+=x;if(result<2)continue outer;}while(result<3);".into()),
        ("review ts-246 classic destructure".into(), "let result=0;for(let [i,j]=[0,1];i<2;i++){result+=i+j;}".into()),
        ("review ts-247 for in destructure".into(), "let result=0;outer:for(const [x] in {abc:1}){result+=x.length;break outer;}".into()),
        ("review ts-248 default middle".into(), "let result=0;switch(3){case 1:result++;default:result+=2;case 3:result+=3;}".into()),
        ("review ts-249 empty switch side effect".into(), "let result=0;switch(result++){}".into()),
        ("review ts-250 default only side effect".into(), "let result=0;switch(result++){default:result+=2;}".into()),
        ("review ts-251 named recursive".into(), "const f=function named(x:number):number {return x?named(x-1):0;};const result=f(2);".into()),
        ("review ts-252 logical index side effects".into(), "let i=0;const xs=[0,1];xs[i++] ||= i++;const result=[i,xs];".into()),
        ("review ts-253 compound index side effects".into(), "let i=0;const xs=[1,2];xs[i++] += i++;const result=[i,xs];".into()),
        ("review ts-254 optional grouping".into(), "const obj={x:{y:1}};const result=(obj?.x).y;".into()),
        ("review ts-255 optional bound call".into(), "const obj={x:1,f:function():number{return this.x;}};const result=(obj?.f)();".into()),
        ("review ts-256 enum expression".into(), "enum E{A=1+2,B=A+1}const result=E.B;".into()),
        ("review ts-257 authored temp".into(), "let t1=1;try{throw 2;}catch(t2){t1+=t2;}".into()),
        ("review ts-258 private sibling scopes".into(), "let result=0;{let x=1;result+=x;}{let x=2;result+=x;}".into()),
        ("audit block-local function simple".into(), "{function f_1(x_1:number):number{return x_1+1;}const result=f_1(1);}".into()),
        ("audit block-local function with local".into(), "{function f_1(x_1:number):number{let x=2;return x+x_1;}const result=f_1(1);}".into()),
    ]
}

/// PRINT-ROUNDTRIP over admitted control and expression compositions.
/// Known failures stay explicit; a fixed case must leave the list.
#[test]
fn enumerated_compositions_print_and_relower() {
    use lash_kernel_dialect::{Environment, Mode};
    let started = std::time::Instant::now();
    let identities = |text: &'static str| {
        let identities = text
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect::<Vec<_>>();
        assert!(
            identities.windows(2).all(|pair| pair[0] < pair[1]),
            "identity lists must be sorted and unique"
        );
        identities
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    };
    let mut known_failures = identities(include_str!("known_print_failures.txt"));
    let expected_refusals = identities(include_str!("composition_lowering_refusals.txt"));
    let mut seen = std::collections::BTreeSet::new();
    let library = super::library();
    let namespaces = Default::default();
    let environment = Environment::new(library, &namespaces);
    let mut forms = flat_source_forms()
        .into_iter()
        .map(|(name, body)| (format!("flat {name}"), body.to_owned()))
        .collect::<Vec<_>>();
    forms.extend(review_source_forms());
    forms.extend(composed_control_forms());
    forms.extend(composed_expression_forms());
    let mut refused = Vec::new();
    let mut failures = Vec::new();
    let mut counts = std::collections::BTreeMap::<String, [usize; 4]>::new();
    for (name, body) in forms {
        for module in [false, true] {
            let context = if module { "module" } else { "cell" };
            let identity = format!("{name}/{context}");
            assert!(seen.insert(identity.clone()), "duplicate case: {identity}");
            let mode = if module {
                Mode::Module
            } else {
                Mode::Cell(lash_kernel_dialect::CellEnvironment::first())
            };
            let source = if module {
                format!("export const entry=async(): Promise<number>=>1; {body}")
            } else {
                body.clone()
            };
            let count = counts
                .entry(name.split_whitespace().next().unwrap().to_owned())
                .or_default();
            let lowered = match crate::lower(&source, &environment, mode) {
                Ok(lowered) => lowered,
                Err(_) => {
                    count[3] += 1;
                    refused.push(identity.clone());
                    continue;
                }
            };
            count[0] += 1;
            let printed = match crate::print(
                &lowered.document,
                Some(&lowered.captions),
                library,
                lash_kernel_dialect::PrintOptions {
                    source_map: true,
                    ..Default::default()
                },
            ) {
                Ok(printed) => printed,
                Err(error) => {
                    count[2] += 1;
                    if !known_failures.remove(identity.as_str()) {
                        failures.push(format!("{identity}: print: {error}\nORIGINAL:\n{source}"));
                    }
                    continue;
                }
            };
            match crate::lower(&printed.source, &environment, mode) {
                Ok(read)
                    if read.document.canonical_bytes() == lowered.document.canonical_bytes() =>
                {
                    count[1] += 1;
                    printed
                        .source_map
                        .as_ref()
                        .unwrap()
                        .validate(&lowered.document, &printed.source)
                        .unwrap();
                }
                Ok(_) => {
                    count[2] += 1;
                    if !known_failures.remove(identity.as_str()) {
                        failures.push(format!(
                            "{identity}: document mismatch\nORIGINAL:\n{source}\nPRINT:\n{}",
                            printed.source
                        ));
                    }
                }
                Err(error) => {
                    count[2] += 1;
                    if !known_failures.remove(identity.as_str()) {
                        failures.push(format!(
                            "{identity}: lower print: {error}\nORIGINAL:\n{source}\nPRINT:\n{}",
                            printed.source
                        ));
                    }
                }
            }
        }
    }
    eprintln!("COMPOSITION COUNTS [admitted, equal, known-failed, lower-refused]: {counts:?}");
    eprintln!("COMPOSITION ELAPSED: {:?}", started.elapsed());
    assert_eq!(
        counts
            .iter()
            .map(|(group, count)| (group.clone(), [count[0], count[3]]))
            .collect::<std::collections::BTreeMap<_, _>>(),
        std::collections::BTreeMap::from([
            ("audit".to_owned(), [4, 0]),
            ("control".to_owned(), [3545, 41]),
            ("flat".to_owned(), [448, 16]),
            ("position".to_owned(), [1860, 44]),
            ("review".to_owned(), [57, 1])
        ]),
        "admission counts must not silently shrink"
    );
    assert_eq!(
        refused
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        expected_refusals,
        "lowering refusals must remain explicit"
    );
    assert!(
        failures.is_empty() && known_failures.is_empty(),
        "UNLISTED PRINT FAILURES: {}\n{}\nKNOWN PRINT FAILURES THAT NOW PASS OR ARE NO LONGER ADMITTED: {}\n{}",
        failures.len(),
        failures.join("\n"),
        known_failures.len(),
        known_failures.into_iter().collect::<Vec<_>>().join("\n")
    );
}

fn composed_control_forms() -> Vec<(String, String)> {
    let controls = [
        ("block", "{$BODY}", 0u8),
        ("if", "if(result<2){$BODY}", 0u8),
        ("if-else", "if(result<2){$BODY}else{result++;}", 0u8),
        ("while", "while(result<2){$BODY}", 3u8),
        ("do", "do{$BODY}while(result<2);", 3u8),
        ("classic-for", "for(let i$N=0;i$N<2;i$N++){$BODY}", 3u8),
        (
            "classic-pattern",
            "for(let [i$N]=[0];i$N<2;i$N+=2){$BODY}",
            3u8,
        ),
        ("for-empty", "for(;;){$BODY}", 3u8),
        ("for-of", "for(const x$N of [1,2]){$BODY}", 3u8),
        (
            "for-of-pattern",
            "for(const [x$N] of [[1],[2]]){$BODY}",
            3u8,
        ),
        ("for-in", "for(const x$N in {a:1,b:2}){$BODY}", 3u8),
        ("for-in-pattern", "for(const [x$N] in {ab:1}){$BODY}", 3u8),
        (
            "switch",
            "switch(result){case 0:$BODY break;default:result++;}",
            2u8,
        ),
        (
            "switch-fallthrough",
            "switch(result){case 0:$BODY case 1:result++;default:result++;}",
            2u8,
        ),
        ("switch-default", "switch(result){default:$BODY}", 2u8),
        ("try-catch", "try{$BODY}catch(error$N){result++;}", 0u8),
        ("try-catch-none", "try{$BODY}catch{result++;}", 0u8),
        ("try-finally", "try{$BODY}finally{result++;}", 0u8),
        (
            "try-catch-finally",
            "try{$BODY}catch(error$N){result++;}finally{result++;}",
            0u8,
        ),
        (
            "closure",
            "const nested$N=():number=>{$BODY return result;};result=nested$N();",
            4u8,
        ),
    ];
    let mut forms = Vec::new();
    for (outer_name, outer, outer_role) in controls {
        for (inner_name, inner, inner_role) in controls {
            let mut exits = vec![
                ("plain", "result++;", false),
                ("return", "return result;", false),
            ];
            if inner_role & 4 == 0 && (inner_role & 2 != 0 || outer_role & 6 == 2) {
                exits.push(("break", "break;", false));
            }
            if inner_role & 4 == 0 && (inner_role & 1 != 0 || outer_role & 5 == 1) {
                exits.push(("continue", "continue;", false));
            }
            if inner_role & 4 == 0 && outer_role & 2 != 0 {
                exits.push(("label-break", "break outer_label;", true));
            }
            if inner_role & 4 == 0 && outer_role & 1 != 0 {
                exits.push(("label-continue", "continue outer_label;", true));
            }
            for (exit, payload, label) in exits {
                let inner = inner.replace("$N", "1").replace("$BODY", payload);
                let outer = outer.replace("$N", "0").replace("$BODY", &inner);
                let outer = if label {
                    format!("outer_label:{outer}")
                } else {
                    outer
                };
                let body = if exit == "return" {
                    format!(
                        "const run=():number=>{{let result=0;{outer}return result;}};const result=run();"
                    )
                } else {
                    format!("let result=0;{outer}")
                };
                forms.push((format!("control {outer_name}/{inner_name}/{exit}"), body));
            }
        }
    }
    forms
}

fn composed_expression_forms() -> Vec<(String, String)> {
    let positions = [
        ("variable-rhs", "target=$EXPR;"),
        ("field-rhs", "box.x=$EXPR;"),
        ("index-rhs", "arr[0]=$EXPR;"),
        ("pattern-rhs", "[target]=[$EXPR];"),
        ("compound-rhs", "target+=$EXPR;"),
        ("logical-rhs", "target||=$EXPR;"),
        ("assignment-key", "box[$EXPR]=1;"),
        ("compound-key", "box[$EXPR]+=1;"),
        ("logical-key", "box[$EXPR]||=1;"),
        ("call-callee", "($EXPR)(1);"),
        ("call-argument", "sink($EXPR);"),
        ("call-receiver", "($EXPR).f(1);"),
        ("call-key", "box[$EXPR](1);"),
        ("call-spread", "sink(...($EXPR));"),
    ];
    let mut forms = Vec::new();
    for (name, body) in flat_source_forms() {
        if !(name.starts_with("expression ")
            || name.starts_with("binary ")
            || name.starts_with("unary "))
        {
            continue;
        }
        let Some((prefix, expression)) = body.rsplit_once("const result=") else {
            panic!("expression form without result: {name}");
        };
        let expression = expression.trim_end_matches(';');
        for (position, template) in positions {
            let body = format!(
                "{prefix}let target:any=0;const box:any={{x:0}};const arr:any=[0];const sink=(value:any):any=>value;{}",
                template.replace("$EXPR", &format!("({expression})"))
            );
            forms.push((format!("position {name}/{position}"), body));
        }
    }
    forms
}
