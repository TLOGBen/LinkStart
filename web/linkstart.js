/* LinkStart v1 browser client: capability remains in closure memory only. */
export function createLinkStartClient({ baseUrl }) {
  let instanceId, capability, cursor = 0;
  const headers = (json = false) => ({ ...(json ? { "Content-Type": "application/json" } : {}), "Authorization": `Bearer ${capability}` });
  async function unwrap(p) { const r = await p, x = await r.json(); if (!r.ok || x.error) throw new Error(x.error || r.statusText); return x; }
  return {
    async redeem(grant) { const x=await unwrap(fetch(`${baseUrl}/v1/launch-grants/redeem`,{method:"POST",headers:{Authorization:`Bearer ${grant}`}})); instanceId=x.instanceId;capability=x.capability;try{window.dispatchEvent(new CustomEvent("linkstart:connected",{detail:{instanceId}}))}catch(_){};return {instanceId,status:x.status}; },
    send(eventId,payload) { return unwrap(fetch(`${baseUrl}/v1/apps/${instanceId}/events`,{method:"POST",headers:headers(true),body:JSON.stringify({eventId,payload})})); },
    async stream(onMessage,signal) { const r=await fetch(`${baseUrl}/v1/apps/${instanceId}/stream?cursor=${cursor}`,{headers:headers(),signal});if(!r.ok)throw new Error("stream_auth_failed");const reader=r.body.pipeThrough(new TextDecoderStream()).getReader();let b="";while(true){const {done,value}=await reader.read();if(done)return;b+=value;let i;while((i=b.indexOf("\n\n"))>=0){const raw=b.slice(0,i);b=b.slice(i+2);const id=(raw.match(/^id: (.+)$/m)||[])[1],kind=(raw.match(/^event: (.+)$/m)||[])[1],data=(raw.match(/^data: (.+)$/m)||[])[1];if(id)cursor=Number(id);if(kind&&data)onMessage(kind,JSON.parse(data));}} },
    state:()=>({instanceId,connected:Boolean(capability),cursor})
  };
}
