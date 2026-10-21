import { load, to_cols } from "df";
export async function nightly(path: string): Promise<number> {
    return to_cols(await load(path)).price.length;
}
