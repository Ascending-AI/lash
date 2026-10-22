import { readCsv } from "io";

export function to_cols(rows: Record<string, number>[]): Record<string, number[]> {
    const cols: Record<string, number[]> = {};
    for (const row of rows) {
        for (const key in row) {
            if (cols[key] === undefined) cols[key] = [];
            cols[key].push(row[key]);
        }
    }
    return cols;
}

export async function load(path: string): Promise<Record<string, number>[]> {
    const text = await readCsv({ path });
    const started = Date.now();
    const lines = text.trim().split("\n");
    const keys = lines[0].split(",");
    const rows: Record<string, number>[] = [];
    for (let line = 1; line < lines.length; line++) {
        const values = lines[line].split(",");
        const row: Record<string, number> = {};
        for (let column = 0; column < keys.length; column++) {
            row[keys[column]] = Number(values[column]);
        }
        rows.push(row);
    }
    return rows;
}
