// LINK START 進場動畫；附加在文件尾端，因為 App html 不保證有 <body> 標籤。
// pointer-events:none —— 動畫絕不能攔截使用者對 App 的第一個互動。
// 隧道段等待 App 發出的 `linkstart:connected` 事件才收尾；等不到由 MAX_TUNNEL/MAXTOTAL 保險絲結束。
pub(crate) const LINK_START_BOOT: &str = r##"
<style>
#linkstart-boot{position:fixed;inset:0;z-index:2147483647;pointer-events:none;background:#05070d;overflow:hidden;opacity:1;transition:opacity .35s ease}
#linkstart-boot.lsboot-done{opacity:0}
#linkstart-boot canvas{position:absolute;inset:0;width:100%;height:100%}
#linkstart-boot .lsboot-text{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;font:italic 800 clamp(2.4rem,9vw,5.5rem)/1 "Segoe UI",system-ui,sans-serif;letter-spacing:.16em;color:#f2f6ff;text-shadow:0 0 14px rgba(90,200,255,.9),0 0 46px rgba(90,160,255,.5);opacity:0;transform:scale(.85)}
@media (prefers-reduced-motion: reduce){#linkstart-boot canvas{display:none}}
</style>
<div id="linkstart-boot" aria-hidden="true"><canvas></canvas><div class="lsboot-text">LINK START</div></div>
<script>
(function(){try{
var boot=document.getElementById("linkstart-boot");if(!boot)return;
var reduced=false;try{reduced=window.matchMedia("(prefers-reduced-motion: reduce)").matches}catch(_){}
var text=boot.querySelector(".lsboot-text");
var canvas=boot.querySelector("canvas");
var ctx=canvas&&canvas.getContext?canvas.getContext("2d"):null;
var T_TEXT=520,T_FLASH=700,MIN_TUNNEL=1900,MAX_TUNNEL=3600,BURST=420,MAXTOTAL=4600;
var start=null,raf=0,finished=false,connected=false,burstAt=null;
var hues=[350,20,45,90,160,190,215,260,300,325];
var parts=[];
function pspawn(){return{a:Math.random()*Math.PI*2,r:Math.random()*1.1,v:.45+Math.random()*1.2,h:hues[Math.floor(Math.random()*hues.length)],w:.7+Math.random()*2}}
function onConnect(){connected=true}
window.addEventListener("linkstart:connected",onConnect);
function finish(){if(finished)return;finished=true;
if(raf)cancelAnimationFrame(raf);
window.removeEventListener("keydown",finish,true);
window.removeEventListener("linkstart:connected",onConnect);
boot.classList.add("lsboot-done");
setTimeout(function(){if(boot.parentNode)boot.parentNode.removeChild(boot)},400)}
function frame(now){
if(finished)return;
if(start===null)start=now;
var t=now-start,dpr=window.devicePixelRatio||1;
var w=canvas.width=canvas.clientWidth*dpr,h=canvas.height=canvas.clientHeight*dpr;
var cx=w/2,cy=h/2,m=Math.hypot(cx,cy)||1,i,p;
if(t<T_TEXT){
ctx.fillStyle="#05070d";ctx.fillRect(0,0,w,h);
text.style.opacity=Math.min(1,t/180);
text.style.transform="scale("+(.85+.15*Math.min(1,t/T_TEXT))+")";
}else if(t<T_FLASH){
text.style.opacity=Math.max(0,1-(t-T_TEXT)/120);
ctx.fillStyle="#fff";ctx.fillRect(0,0,w,h);
}else if(burstAt===null){
text.style.opacity=0;
if((connected&&t>MIN_TUNNEL)||t>MAX_TUNNEL){burstAt=t}
else{
var tt=t-T_FLASH;
ctx.fillStyle="#fff";ctx.fillRect(0,0,w,h);
var ramp=Math.min(1,tt/500);
var speed=ramp*(1+.25*Math.sin(t*.006));
while(parts.length<220)parts.push(pspawn());
ctx.lineCap="round";
for(i=0;i<parts.length;i++){p=parts[i];
p.r+=p.v*speed*.02;
if(p.r>1.2){parts[i]=p=pspawn();p.r=.02+Math.random()*.05}
var len=(.06+.3*speed)*(.35+p.r);
var r0=p.r*m,r1=Math.min(p.r+len,1.35)*m;
var x0=cx+Math.cos(p.a)*r0,y0=cy+Math.sin(p.a)*r0;
var x1=cx+Math.cos(p.a)*r1,y1=cy+Math.sin(p.a)*r1;
var al=Math.min(.95,.25+p.r*.75);
var g=ctx.createLinearGradient(x0,y0,x1,y1);
g.addColorStop(0,"hsla("+p.h+",95%,60%,0)");
g.addColorStop(1,"hsla("+p.h+",95%,52%,"+al+")");
ctx.strokeStyle=g;
ctx.globalAlpha=.25;ctx.lineWidth=p.w*2.8*dpr;
ctx.beginPath();ctx.moveTo(x0,y0);ctx.lineTo(x1,y1);ctx.stroke();
ctx.globalAlpha=1;ctx.lineWidth=p.w*dpr;
ctx.beginPath();ctx.moveTo(x0,y0);ctx.lineTo(x1,y1);ctx.stroke()}
var glow=ctx.createRadialGradient(cx,cy,0,cx,cy,(.16+.02*Math.sin(t*.008))*m);
glow.addColorStop(0,"rgba(255,255,255,.95)");glow.addColorStop(1,"rgba(255,255,255,0)");
ctx.fillStyle=glow;ctx.fillRect(0,0,w,h);
}
}
if(burstAt!==null){
var tb=(t-burstAt)/BURST;
if(tb>=1){finish();return}
ctx.fillStyle="#fff";ctx.fillRect(0,0,w,h);
for(var j=0;j<90;j++){var ba=j/90*Math.PI*2;
var bg=ctx.createLinearGradient(cx,cy,cx+Math.cos(ba)*1.4*m,cy+Math.sin(ba)*1.4*m);
bg.addColorStop(0,"rgba(120,225,255,"+.55*(1-tb)+")");
bg.addColorStop(1,"rgba(160,235,255,0)");
ctx.strokeStyle=bg;ctx.lineWidth=(1+j%4)*dpr;
ctx.beginPath();ctx.moveTo(cx,cy);ctx.lineTo(cx+Math.cos(ba)*1.4*m,cy+Math.sin(ba)*1.4*m);ctx.stroke()}
var core=ctx.createRadialGradient(cx,cy,0,cx,cy,(.25+.95*tb)*m);
core.addColorStop(0,"#fff");core.addColorStop(.55,"rgba(255,255,255,.92)");core.addColorStop(1,"rgba(220,246,255,0)");
ctx.fillStyle=core;ctx.fillRect(0,0,w,h);
}
raf=requestAnimationFrame(frame)}
window.addEventListener("keydown",finish,true);
if(reduced||!ctx){text.style.opacity=1;text.style.transform="scale(1)";setTimeout(finish,700)}
else{raf=requestAnimationFrame(frame)}
setTimeout(finish,MAXTOTAL)
}catch(_){}})();
</script>
"##;
pub(crate) fn with_link_start_boot(mut html: String) -> String {
    if html.contains("data-linkstart-boot=\"off\"") {
        return html;
    }
    html.push_str(LINK_START_BOOT);
    html
}
