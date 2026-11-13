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
    let forms = [
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
    ];
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
