import { load, to_cols } from "df";
const rows = await load("s3://sales.csv");
const cols = to_cols(rows);
cols.price.reduce((total, price) => total + price, 0);
