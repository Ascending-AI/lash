let text = "";
const parts = [];
for (let n = 0; n < 500; n++) {
  const part = "row-" + String(n);
  text = text + part;
  parts.push(part);
}
const result = [text, parts.join("|")];
