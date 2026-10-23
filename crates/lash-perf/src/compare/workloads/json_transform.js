function benchmark(n) {
const records=[]; for(let i=0;i<128;i++) records.push({id:i,name:"tool-record-"+i,value:i,active:i%2===0});
const input=JSON.stringify({tool:"search",records:records});
let result=0;
for(let trip=0;trip<n;trip++) {
const data=JSON.parse(input);
for(let i=0;i<data.records.length;i++) data.records[i].value+=1;
result+=JSON.stringify(data).length;
}
return result;
}
