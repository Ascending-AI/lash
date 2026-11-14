
const out = await workflows.create({name:'nr1-numeric-label',run:async ():Promise<unknown>=>{
 let label='';
 for(let i=0;i<3;i++){label='Week '+(i+1);}
 return views.text(label);
}}); console.log(out);
