//! The single HTML page. Kept as a constant so the binary needs no asset
//! directory: `scp` one file to a container and the UI works offline.
//!
//! The dashboard answers one question at a glance — which guest is in trouble.
//! Cards are ordered by identity (host, then VMs, then containers, each by id)
//! so nothing moves between polls, and each card leads with the host label
//! because that is what a person scans for; the node id sits underneath as the
//! stable reference.

pub const INDEX_HTML: &str = r##"<!doctype html>
<html lang="zh"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>YT-Monitor</title>
<link rel="icon" href="/icon-32.png" type="image/png" sizes="32x32">
<link rel="apple-touch-icon" href="/icon-180.png" sizes="180x180">
<meta name="theme-color" content="#0a0c0f">
<style>
:root{--bg:#0a0c0f;--card:#13161c;--card2:#181c23;--line:#232833;--fg:#e8ecf2;
      --dim:#77808f;--dim2:#5b6472;--ok:#3fb950;--warn:#d29922;--bad:#f85149}
*{box-sizing:border-box}
body{margin:0;padding:26px 24px 40px;background:var(--bg);color:var(--fg);
     font:14px/1.5 system-ui,-apple-system,"Segoe UI","Noto Sans SC",sans-serif;
     -webkit-font-smoothing:antialiased}
header{display:flex;align-items:center;gap:14px;margin:0 0 24px;flex-wrap:wrap}
h1{font-size:16px;margin:0;font-weight:600;letter-spacing:.6px}
.sum{display:flex;gap:16px;margin-left:auto;font-size:12.5px;
     font-variant-numeric:tabular-nums;color:var(--dim)}
.sum b{color:var(--fg);font-weight:600}
.sum .bad{color:var(--bad)}
.err{color:var(--bad);margin:0 0 14px;font-size:13px}
.grp{display:flex;align-items:center;gap:9px;margin:26px 0 11px;font-size:12px;
     color:var(--dim);letter-spacing:.1em}
.grp:first-of-type{margin-top:0}
.grp::before{content:'';width:3px;height:12px;border-radius:2px;background:var(--line)}
.grp b{color:var(--fg);font-weight:600;letter-spacing:.05em}
.grp span{color:var(--dim2);font-variant-numeric:tabular-nums}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(238px,1fr));gap:11px}
.card{background:var(--card);border:1px solid var(--line);border-radius:11px;
      padding:14px 15px 13px;transition:border-color .18s,background .18s}
.card:hover{background:var(--card2);border-color:#2d3441}
.card.warn{border-left:2px solid var(--warn);padding-left:14px}
.card.crit{border-left:2px solid var(--bad);padding-left:14px}
.card.off{opacity:.62}
.card.off:hover{opacity:.8}
.top{display:flex;align-items:center;gap:8px;margin:0 0 3px}
.name{font-size:15px;font-weight:600;letter-spacing:.2px;line-height:1.3;
      overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.card.off .name{color:var(--dim)}
.dot{width:6px;height:6px;border-radius:50%;flex:none;margin-left:auto}
.sub{display:flex;gap:7px;font-size:11.5px;color:var(--dim2);margin:0 0 13px;
     font-variant-numeric:tabular-nums;letter-spacing:.02em}
.sub .ip{margin-left:auto;color:var(--dim)}
.sub .offnote{color:var(--bad)}
.m{display:flex;align-items:baseline;justify-content:space-between;
   font-size:12px;color:var(--dim);margin:0 0 4px}
.m b{color:var(--fg);font-weight:500;font-size:12.5px;font-variant-numeric:tabular-nums}
.m b.na{color:var(--dim2);font-weight:400}
.bar{height:4px;background:#0b0e12;border-radius:2px;overflow:hidden;margin:0 0 10px}
.bar:last-child{margin-bottom:0}
.bar i{display:block;height:100%;border-radius:2px;transition:width .5s cubic-bezier(.4,0,.2,1)}
.empty{color:var(--dim);padding:40px;text-align:center;font-size:13px}
</style></head><body>
<header>
  <h1>YT-Monitor</h1>
  <div class="sum"><span><b id="on">—</b> 在线</span><span><b id="tot">—</b> 总数</span>
    <span id="alertBox" style="display:none"><b class="bad" id="alerts">0</b> 异常</span>
    <span id="stat">—</span></div>
</header>
<div class="err" id="err"></div>
<div id="grid"></div>
<script>
const $=id=>document.getElementById(id);
const col=p=>p>=90?'var(--bad)':p>=70?'var(--warn)':'var(--ok)';
const pct=v=>v==null?'—':(v>0&&v<1?'<1%':v.toFixed(0)+'%');
const age=s=>{if(s==null||s>=1e18)return '未知';
  if(s<60)return s+' 秒';
  if(s<3600)return Math.round(s/60)+' 分钟';
  if(s<86400)return Math.round(s/3600)+' 小时';
  return Math.round(s/86400)+' 天'};
function metric(label,v){
  const na=v==null, p=na?0:Math.max(0,Math.min(100,v));
  const c=na?'transparent':col(p);
  return '<div class="m"><span>'+label+'</span><b'+(na?' class="na"':'')+'>'+pct(v)+'</b></div>'
       + '<div class="bar"><i style="width:'+p+'%;background:'+c+'"></i></div>';
}
function card(h){
  const on=h.online;
  // Worst metric decides the card's edge: a guest at 91% CPU should be findable
  // without reading three bars.
  const worst=Math.max(h.cpu==null?-1:h.cpu, h.mem==null?-1:h.mem, h.disk==null?-1:h.disk);
  const state=!on?' off':(worst>=90?' crit':(worst>=70?' warn':''));
  return '<div class="card'+state+'">'
    + '<div class="top"><span class="name">'+(h.host||h.node_id||'?')+'</span>'
    + '<span class="dot" style="background:'+(on?'var(--ok)':'var(--bad)')+'"></span></div>'
    + '<div class="sub"><span>'+(h.node_id||'')+'</span>'
    + (on?'<span class="ip">'+(h.ip||'—')+'</span>'
        :'<span class="ip offnote">离线 · '+age(h.age)+'</span>')
    + '</div>'
    + metric('CPU',h.cpu) + metric('内存',h.mem) + metric('磁盘',h.disk)
    + '</div>';
}
const GROUPS=[['host','宿主机'],['vm','虚拟机'],['ct','容器']];
const groupOf=id=>{const g=String(id||'').split(':')[0];
  return GROUPS.some(x=>x[0]===g)?g:'ct';};
function render(hs){
  let out='';
  for(const [key,title] of GROUPS){
    const items=hs.filter(h=>groupOf(h.node_id)===key);
    if(!items.length) continue;
    out+='<div class="grp"><b>'+title+'</b><span>'+items.length+' 台</span></div>';
    out+='<div class="grid">'+items.map(card).join('')+'</div>';
  }
  return out||'<div class="empty">还没有主机上报</div>';
}
async function load(){
  try{
    const d=await (await fetch('/api/overview')).json();
    const hs=d.hosts||[];
    const online=hs.filter(x=>x.online).length;
    const bad=hs.filter(x=>x.online&&Math.max(x.cpu==null?-1:x.cpu,x.mem==null?-1:x.mem,
                                             x.disk==null?-1:x.disk)>=90).length;
    $('on').textContent=online; $('tot').textContent=hs.length;
    if(bad){$('alertBox').style.display='';$('alerts').textContent=bad;}
    else $('alertBox').style.display='none';
    $('stat').textContent=new Date().toLocaleTimeString('zh-CN',{hour12:false});
    $('grid').innerHTML=render(hs);
    $('err').textContent='';
  }catch(e){$('err').textContent='加载失败：'+e.message;}
}
load();setInterval(load,15000);
</script></body></html>"##;
