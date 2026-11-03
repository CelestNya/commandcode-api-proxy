/* navcheck: 导航激活项的实际像素对比度
 * 探针的 computed-style 解析读不了 linear-gradient 的色标，
 * 直接从截图里采样：激活项底色 + 白色文字笔画的对比。 */
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
const lum=(c)=>{const f=v=>{v/=255;return v<=0.03928?v/12.92:Math.pow((v+0.055)/1.055,2.4)};return 0.2126*f(c[0])+0.7152*f(c[1])+0.0722*f(c[2])};
const ratio=(a,b)=>{const l1=lum(a),l2=lum(b);return (Math.max(l1,l2)+0.05)/(Math.min(l1,l2)+0.05);};
const img=decodePNG(fs.readFileSync(".probe/shots/jf-jelly-1440.png"));
// 激活导航项在左侧栏第一项，约 y=68, x=10..170
let bg=null, text=null;
for(let y=58;y<80;y++)for(let x=8;x<180;x++){
  const c=px(img,x,y);
  const lumv=lum(c);
  if(lumv>0.80){ if(!bg||lum(c)<lum(bg)) bg=c; }          // 纯白笔画
  else if(lumv>0.12 && lumv<0.22){ if(!text||lum(c)<lum(text)) text=c; } // 粉底
}
console.log("激活项 白字笔画 rgb("+bg+") | 底色 rgb("+text+")");
console.log("对比度 "+ratio(bg,text).toFixed(2)+":1");
