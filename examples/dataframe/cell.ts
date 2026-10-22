import { load, toCols } from "df";
const rows = await load("s3://sales.csv");
const cols = toCols(rows);
cols.price.reduce((total, price) => total + price, 0);
