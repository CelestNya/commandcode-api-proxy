/* blobscan: 只统计"亮像素"（背景），排除文字与卡片边框的干扰，
 * 对比参考与我们在空白区的底色分布与光斑色相跨度。 */
const fs = require("fs"), zlib = require("zlib");
function decodePNG(buf){let pos=8,w=0,h=0,ct=0;const idat=[];
while(pos<buf.length){const len=buf.readUInt32BE(pos);const type=buf.toString("ascii",pos+4,pos+8);const data=buf.slice(pos+8,pos+8+len);
if(type==="IHDR"){w=data.readUInt32BE(0);h=data.readUInt32BE(4);ct=data[9];}else if(type==="IDAT")idat.push(data);else if(type==="IEND")break;pos+=12+len;}
const ch=ct===6?4:ct===2?3:0;const raw=zlib.inflateSync(Buffer.concat(idat));const stride=w*ch;const out=Buffer.alloc(h*stride);let rp=0;
for(let y=0;y<h;y++){const ft=raw[rp++];const row=raw.slice(rp,rp+stride);rp+=stride;const cur=out.slice(y*stride,(y+1)*stride);const prev=y>0?out.slice((y-1)*stride,y*stride):Buffer.alloc(stride);
for(let x=0;x<stride;x++){const a=x>=ch?cur[x-ch]:0;const b=prev[x];const c=x>=ch?prev[x-ch]:0;let v=row[x];
if(ft===1)v+=a;else if(ft===2)v+=b;else if(ft===3)v+=(a+b)>>1;else if(ft===4){const p=a+b-c,pa=Math.abs(p-a),pb=Math.abs(p-b),pc=Math.abs(p-c);v+=(pa<=pb&&pa<=pc)?a:(pb<=pc?b:c);}cur[x]=v&255;}}
return{w,h,ch,data:out};}
const px=(im,x,y)=>{const i=y*im.w*im.ch+x*im.ch;return[im.data[i],im.data[i+1],im.data[i+2]];};
function scan(img,label,region){
  const [rx0,ry0,rx1,ry1]=region;
  let sum=[0,0,0],n=0,min=[255,255,255],max=[0,0,0];
  for(let y=Math.round(ry0*img.h);y<Math.round(ry1*img.h);y+=2){
    for(let x=Math.round(rx0*img.w);x<Math.round(rx1*img.w);x+=2){
      const c=px(img,x,y);
      if(Math.min(...c)<200) continue;   // 排除文字与深色边框
      for(let i=0;i<3;i++){sum[i]+=c[i];min[i]=Math.min(min[i],c[i]);max[i]=Math.max(max[i],c[i]);}
      n++;
    }
  }
  const avg=sum.map(v=>Math.round(v/n));
  console.log(`${label}  样本${n}
  平均 rgb(${avg})  范围 R${min[0]}~${max[0]} G${min[1]}~${max[1]} B${min[2]}~${max[2]}`);
}
const ref=decodePNG(fs.readFileSync(".probe/shots/ref-dashboard.png"));
const our=decodePNG(fs.readFileSync(".probe/shots/jf-jelly-attempts.png"));
scan(ref,"参考·下方空白带",[0.20,0.72,0.95,0.98]);
scan(our,"我们·下方空白带",[0.20,0.45,0.95,0.97]);
scan(ref,"参考·侧栏外空白",[0.00,0.85,0.12,0.98]);
scan(our,"我们·侧栏外空白",[0.00,0.60,0.11,0.98]);
