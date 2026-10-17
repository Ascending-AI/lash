export async function demo(name) {
  let t7 = async function(this1, args1) {
    let t1 = args1;
    if (helper_list_len(args1) < 1.0) {
      t1 = await helper_ts_pad(args1, 1.0);
    }
    let name_1 = t1[0.0];
    let größe = "日本 🦀";
    let t4 = helper_text_concat(größe, name_1);
    let t5 = await helper_ts_boundary(t4);
    let t6 = await await tools.corpus.echo(t5);
    return t6;
  };
  let t8 = [name];
  let t9 = await (t7)(undefined, t8);
  if (helper_same(t9, undefined)) {
    return null;
  }
  return t9;
}
async function main() {
  let demo = declared.demo;
}
