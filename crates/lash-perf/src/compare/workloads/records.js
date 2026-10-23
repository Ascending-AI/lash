function benchmark(n) {
const records=[]; for(let i=0;i<10000;i++) records.push({value:i,active:i%2===0});
let result=0;
for(let trip=0;trip<n;trip++) result+=records.filter(x=>x.active).map(x=>x.value*2).reduce((a,b)=>a+b,0);
return result;
}
