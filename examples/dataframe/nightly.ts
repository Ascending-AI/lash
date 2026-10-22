import { load, toCols } from "df";
export async function nightly(path: string): Promise<number> {
    return toCols(await load(path)).price.length;
}
