// Live Node oracle for FIG-6372. JSON lines in, canonical tagged JSON out.
import readline from 'node:readline';
import vm from 'node:vm';
function tag(value) {
  if (value === null) return {type:'null'};
  if (value === undefined) return {type:'undefined'};
  if (typeof value === 'boolean') return {type:'bool', value};
  if (typeof value === 'number') {
    const bytes = Buffer.alloc(8);
    bytes.writeDoubleBE(Number.isNaN(value) ? NaN : value);
    return {type:'float', bits:bytes.toString('hex')};
  }
  if (typeof value === 'string') return {type:'str', value:Array.from({length:value.length},(_,i)=>value.charCodeAt(i))};
  if (Array.isArray(value)) return {type:'list', value:Array.from(value, tag)};
  if (Object.prototype.toString.call(value) === '[object Set]') return {type:'set',value:Array.from(value,tag)};
  if (Object.prototype.toString.call(value) === '[object Map]') return {type:'dict',value:Array.from(value,([k,v])=>[tag(k),tag(v)])};
  if (typeof value === 'object') return {type:'object',value:Object.entries(value).map(([k,v])=>[k,tag(v)])};
  throw new Error(`Unrecordable value ${typeof value}`);
}
for await (const line of readline.createInterface({input:process.stdin})) {
  // Preserve the full integer host payload before JavaScript Number can
  // round a u64 bit source or nanosecond timestamp (the oracle is Node 25).
  const c = JSON.parse(line, (key, value, context) =>
    ['bits', 'stride', 'nanoseconds'].includes(key) && typeof value === 'number'
      ? context.source : value);
  const values = [];
  const context = vm.createContext({host:{echo:v=>JSON.parse(JSON.stringify(v, (_key,value)=>typeof value === 'bigint' ? JSON.rawJSON(String(value)) : value)), observe:v=>{values.push(tag(v)); return null;}}, console:{log:()=>{},debug:()=>{},info:()=>{},warn:()=>{},error:()=>{}}, URL, URLSearchParams});
  // Only these explicit host reads are deterministic. All other semantics
  // remain Node's. UTC is a property of the oracle process (TZ=UTC).
  vm.runInContext(`{
    let bits = BigInt('${c.bits ?? 0}');
    const stride = BigInt('${c.stride ?? 0}');
    Math.random = () => { const value = Number(bits >> 11n) * 2 ** -53; bits = BigInt.asUintN(64,bits+stride); return value; };
    const NativeDate = Date;
    const ms = Number(BigInt('${c.nanoseconds ?? 0}') / 1000000n);
    function FixedDate(...args) { if (new.target) return new NativeDate(...(args.length ? args : [ms])); return new NativeDate(ms).toString(); }
    FixedDate.prototype = NativeDate.prototype;
    Object.setPrototypeOf(FixedDate, NativeDate);
    FixedDate.now = () => ms;
    globalThis.Date = FixedDate;
  }`, context);
  let result;
  try {
    await new vm.Script(`(async () => { ${c.source}\n})()`).runInContext(context,{timeout:1000});
    result = {type:'observations',value:values};
  } catch (error) { result = typeof error?.name === 'string' ? {type:'throw',value:error.name} : {type:'throw_value',value:tag(error)}; }
  process.stdout.write(JSON.stringify({id:c.id,result})+'\n');
}
