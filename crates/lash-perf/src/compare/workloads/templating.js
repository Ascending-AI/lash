function benchmark(n) {
let result=0;
for(let trip=0;trip<n;trip++) {
let text="";
for(let i=0;i<100;i++) text+=`Tool ${i}: value=${i*3}; status=ready
`;
result+=text.length;
}
return result;
}
