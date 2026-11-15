
const out = await workflows.create({name:'nr1-method-argument',run:async ():Promise<unknown>=>{
 const result=await cenote.readDocument({document:{doc_id:'01900000-0000-7000-8000-000000000001'}});
 if(!result.ok) throw new Error('Cannot read');
 const citations:Array<string>=[];
 citations.push('['+result.data.title+']');
 return views.text(citations.join(' '));
}}); console.log(out);
