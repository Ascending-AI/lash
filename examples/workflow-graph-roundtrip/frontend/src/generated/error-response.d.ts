/* Generated from the example Rust HTTP DTOs by npm run generate:types. Do not edit directly. */

export interface ErrorBody {
  error: ErrorDetail;
  [k: string]: unknown;
}
export interface ErrorDetail {
  code: string;
  details: unknown;
  message: string;
  [k: string]: unknown;
}
