// json-schema-to-typescript consumes Draft 7 tuples and reference constraints.
// Translate only its compiler input; published Draft 2020-12 documents stay intact.
export function schemaForTypescript(schema) {
  if (Array.isArray(schema)) return schema.map(schemaForTypescript);
  if (schema === null || typeof schema !== 'object') return schema;
  const result = Object.fromEntries(
    Object.entries(schema).map(([key, value]) => [
      key,
      schemaForTypescript(value),
    ]),
  );
  if (Array.isArray(result.prefixItems)) {
    if (result.items !== undefined) result.additionalItems = result.items;
    else if (result.maxItems === undefined) result.additionalItems = true;
    result.items = result.prefixItems;
    delete result.prefixItems;
  }
  if (typeof result.$ref === 'string' && Object.keys(result).length > 1) {
    const { $ref, ...siblings } = result;
    const annotations = {};
    const constraints = {};
    for (const [key, value] of Object.entries(siblings)) {
      if (
        [
          'title',
          'description',
          'default',
          'examples',
          'deprecated',
          'readOnly',
          'writeOnly',
        ].includes(key)
      )
        annotations[key] = value;
      else constraints[key] = value;
    }
    return {
      ...annotations,
      allOf: [
        { $ref },
        ...(Object.keys(constraints).length ? [constraints] : []),
      ],
    };
  }
  return result;
}
