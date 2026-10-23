//! Chosen runnable module examples plus repeated-boundary witnesses.
use serde_json::{Value, json};
pub struct Case {
    pub name: &'static str,
    pub source: &'static str,
    pub dialect: &'static str,
    pub minimum: usize,
}
impl Case {
    pub fn language(&self) -> lash::workflow::document::SourceLanguage {
        if self.dialect == "typescript" {
            lash::workflow::document::SourceLanguage::TypeScript
        } else {
            lash::workflow::document::SourceLanguage::Python
        }
    }
    pub fn args(&self) -> Value {
        json!({})
    }
}
pub fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for row in workflow_corpus::SOURCES {
        let minimum = match row.name {
            "straight-tool" | "fan-out" | "sleep" => 1,
            "try-finally" => 2,
            _ => continue,
        };
        for (dialect, source) in [("typescript", row.typescript), ("python", row.python)] {
            if let Some(source) = source {
                cases.push(Case {
                    name: row.name,
                    source,
                    dialect,
                    minimum,
                });
            }
        }
    }
    cases.extend([
        Case {
            name: "repeated.ts",
            dialect: "typescript",
            minimum: 9,
            source: r#"
export async function demo(): Promise<string> {
  const captured = [7];
  const closure = async (n: number) => {
    await sleep(10000);
    return captured[0] + n;
  };
  const values = [];
  for (let n = 0; n < 3; n++) {
    const value = await closure(n);
    const echoed = await corpus.echo(value);
    const input = await corpus.input(echoed);
    values.push([echoed, input]);
  }
  return JSON.stringify(values);
}
"#,
        },
        Case {
            name: "repeated.py",
            dialect: "python",
            minimum: 9,
            source: r#"
import asyncio

async def demo() -> str:
    captured = [7]
    async def closure(n):
        await asyncio.sleep(10)
        return captured[0] + n
    values = []
    for n in range(3):
        value = await closure(n)
        echoed = await corpus.echo(value)
        answer = await corpus.input(echoed)
        values.append([echoed, answer])
    return str(values)
"#,
        },
        Case {
            name: "nested.ts",
            dialect: "typescript",
            minimum: 3,
            source: r#"
export async function demo(): Promise<string> {
  const child = async (): Promise<string> => {
    await sleep(10000);
    const echoed = await corpus.echo(42);
    return await corpus.input(echoed);
  };
  const handle = await processes.start({definition: child});
  return await handle;
}
"#,
        },
        Case {
            name: "nested.py",
            dialect: "python",
            minimum: 3,
            source: r#"
import asyncio
async def demo() -> str:
    async def child() -> str:
        await asyncio.sleep(10)
        echoed = await corpus.echo(42)
        return await corpus.input(echoed)
    handle = await processes.start({"definition": child})
    return await getattr(processes, "await")({"handle": handle})
"#,
        },
    ]);
    cases
}
