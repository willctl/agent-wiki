"""Render the social demo from fictional notes and an isolated app capture.

Requires Python 3, Pillow, numpy and imageio-ffmpeg. No network or live wiki access.
Set SHOWCASE_FONT / SHOWCASE_FONT_BOLD to use another installed sans-serif font.
"""
from pathlib import Path
import math
import os
import subprocess
import wave
import numpy as np
from PIL import Image, ImageDraw, ImageFont
import imageio_ffmpeg

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'docs' / 'showcase'
OUT.mkdir(parents=True, exist_ok=True)
W, H, FPS, DURATION = 1080, 1350, 24, 32
FONT = os.environ.get('SHOWCASE_FONT', 'C:/Windows/Fonts/segoeui.ttf')
BOLD = os.environ.get('SHOWCASE_FONT_BOLD', 'C:/Windows/Fonts/segoeuib.ttf')
fonts = {}
WHITE, MUTED, VIOLET, MINT = '#f1f2fa', '#a6b2cd', '#b9a8ff', '#80e0ca'

def font(size, bold=False):
    key = size, bold
    if key not in fonts:
        fonts[key] = ImageFont.truetype(BOLD if bold else FONT, size)
    return fonts[key]

def text(d, pos, s, size=30, color=WHITE, bold=False, anchor=None):
    d.text(pos, s, font=font(size, bold), fill=color, anchor=anchor)

def ease(v):
    v = max(0, min(1, v))
    return 1 - (1-v)**3

def card(d, box, fill='#171d30', edge='#37415e', radius=28):
    d.rounded_rectangle(tuple(map(int, box)), radius, fill=fill, outline=edge, width=2)

def icon(d, x, y, size=100):
    s = size / 100
    def p(a,b): return (x+a*s,y+b*s)
    d.rounded_rectangle((p(5,0),p(90,88)), int(17*s), fill='#9787ef')
    d.polygon([p(17,78),p(17,105),p(45,78)], fill='#9787ef')
    d.polygon([p(62,0),p(90,28),p(62,28)], fill='#d5cdff')
    for yy, length in [(35,31),(50,55),(65,41)]:
        d.line([p(22,yy),p(22+length,yy)],fill='white',width=max(2,int(5*s)))

yy, xx = np.mgrid[0:H,0:W]
glow = np.exp(-((xx-850)**2+(yy-550)**2)/360000)
base = np.empty((H,W,3),dtype=np.uint8)
for c,b in enumerate([8,12,23]): base[:,:,c] = b+glow*[22,17,43][c]
BG = Image.fromarray(base)
capture = Image.open(OUT/'demo-page.png').convert('RGB').crop((0,150,449,645))

def background(t):
    im = BG.copy(); d = ImageDraw.Draw(im)
    for x in range(0,W,60): d.line((x,0,x,H), fill='#151b2c')
    for y in range(0,H,60): d.line((0,y,W,y), fill='#151b2c')
    for i in range(24):
        x = (i*137+30*math.sin(t*.2+i))%W
        y = (i*199-t*10)%H
        d.ellipse((x,y,x+2,y+2),fill='#5a597e')
    icon(d,64,62,40); text(d,(118,61),'Agent Wiki',30,bold=True)
    text(d,(1010,71),'DEMO DATA',18,MUTED,anchor='ra')
    d.line((64,1260,1016,1260),fill='#32394f',width=2)
    d.line((64,1260,64+952*t/DURATION,1260),fill=VIOLET,width=3)
    text(d,(64,1290),'SHARED MEMORY / PLAIN MARKDOWN',17,MUTED)
    text(d,(1016,1290),f'{min(5,int(t//6.4)+1):02} / 05',17,MUTED,anchor='ra')
    return im

def scene(index, u, t):
    im = background(t); d = ImageDraw.Draw(im)
    rise = int(38*(1-ease(u/1.1)))
    if index == 0:
        text(d,(64,210+rise),'A little less',80,bold=True)
        text(d,(64,305+rise),'starting over.',80,VIOLET,True)
        text(d,(68,445),'Shared memory for your AI apps.',35,MUTED)
        cx,cy = 540,850
        for i,(label,angle) in enumerate([('Claude',-150),('ChatGPT',-30),('Codex',90)]):
            a=math.radians(angle)+.04*math.sin(t*.6)
            x,y = cx+330*math.cos(a),cy+250*math.sin(a)
            d.line((x,y,cx,cy),fill='#5a527f',width=3)
            q=(u*.24+i*.32)%1
            px,py=x+(cx-x)*q,y+(cy-y)*q
            d.ellipse((px-6,py-6,px+6,py+6),fill=MINT)
            card(d,(x-104,y-38,x+104,y+38))
            text(d,(x,y-20),label,29,anchor='ma')
        card(d,(440,750,640,950),fill='#27213e',edge='#706298',radius=44)
        icon(d,485,795,110)
    elif index == 1:
        text(d,(64,210+rise),'Keep the useful bits.',67,bold=True)
        text(d,(68,315),'Decisions. Preferences. What comes next.',30,MUTED)
        notes=[('A decision','Keep the first version small.'),('A preference','Warm colors. Plain language.'),('The next step','Pick three photos for the story.')]
        for i,(label,body) in enumerate(notes):
            z=ease((u-i*.45)/.85); x=64+int((1-z)*180); y=460+i*207
            card(d,(x,y,1016+int((1-z)*180),y+168))
            text(d,(x+32,y+25),label.upper(),19,VIOLET)
            text(d,(x+32,y+72),body,35,bold=True)
            d.ellipse((x+895,y+28,x+907,y+40),fill=MINT)
        text(d,(68,1150),'One place to pick up the thread.',33,MUTED)
    elif index == 2:
        text(d,(64,190+rise),'Find it again.',76,bold=True)
        card(d,(64,310,1016,392),fill='#20243a')
        query='weekend studio'; typed=query[:min(len(query),int(max(0,u-.35)*15))]
        d.ellipse((89,335,109,355),outline=MUTED,width=2); d.line((108,354,118,365),fill=MUTED,width=2)
        text(d,(139,326),typed,31)
        if u<1.7: d.line((143+d.textlength(typed,font=font(31)),330,143+d.textlength(typed,font=font(31)),366),fill=VIOLET,width=2)
        offset=int(70*(1-ease((u-.8)/.9)))
        shot=capture.resize((650,717),Image.Resampling.LANCZOS)
        card(d,(202,438+offset,878,1181+offset),edge='#645987',radius=20)
        im.paste(shot,(215,451+offset))
        text(d,(540,1200),'ACTUAL APP • FICTIONAL NOTES',18,MUTED,anchor='ma')
    elif index == 3:
        text(d,(64,205+rise),'You get a say.',76,bold=True)
        text(d,(68,310),'Use manual approval to review changes.',32,MUTED)
        card(d,(64,462,1016,1038))
        text(d,(104,506),'WEEKEND STUDIO',21,VIOLET)
        text(d,(104,570),'A proposed update',44,bold=True)
        text(d,(104,662),'Publish one story each week.',34)
        text(d,(104,718),'Start with the three photos you picked.',32,MUTED)
        for i,(label,color) in enumerate([('Review',VIOLET),('Keep',MINT),('Undo',MUTED)]):
            x=104+i*287
            card(d,(x,860,x+250,954),fill='#24283e',edge=color)
            text(d,(x+125,884),label,30,color,anchor='ma')
        q=ease((u-1)/1.3)
        d.line((104,803,104+850*q,803),fill=MINT,width=3)
        text(d,(68,1127),'Your notes stay readable as Markdown.',33,MUTED)
        text(d,(68,1180),'ILLUSTRATED WORKFLOW',17,MUTED)
    else:
        icon(d,452,270+rise,190)
        text(d,(540,565),'Agent Wiki',92,bold=True,anchor='ma')
        text(d,(540,710),'A little more continuity.',43,VIOLET,anchor='ma')
        text(d,(540,780),'A little less explaining it again.',38,MUTED,anchor='ma')
        card(d,(155,960,925,1052),edge='#5c527a')
        text(d,(540,984),'github.com/willctl/agent-wiki',32,anchor='ma')
        text(d,(540,1130),'Read the project. See if it fits.',29,MUTED,anchor='ma')
    return im

def frame(t):
    i=min(4,int(t/6.4)); u=t-i*6.4
    im=scene(i,u,t)
    if u<.48 and i>0:
        im=Image.blend(scene(i-1,6.4+u,t),im,ease(u/.48))
    if t<.65: im=Image.blend(Image.new('RGB',(W,H),'#080c17'),im,ease(t/.65))
    return im

# Original, quiet instrumental bed: soft chords, a pluck and a light pulse. No samples.
rate=48000
times=np.arange(rate*DURATION)/rate
audio=np.zeros_like(times)
for start,notes in [(0,[146.83,220,261.63]),(8,[130.81,196,246.94]),(16,[174.61,220,261.63]),(24,[146.83,220,293.66])]:
    z=times-start
    env=np.clip(z/1.2,0,1)*np.clip((9-z)/2,0,1)*(z>=0)
    for freq in notes: audio+=.035*env*np.sin(2*np.pi*freq*times+.1*np.sin(times*.7))
for beat in np.arange(.5,31,.5):
    z=times-beat; env=np.exp(-np.maximum(z,0)*8)*(z>=0)
    freq=[440,523.25,659.25,587.33][int(beat*2)%4]
    audio+=.026*env*np.sin(2*np.pi*freq*z)
audio*=np.minimum(1,times/2)*np.minimum(1,(DURATION-times)/2)
stereo=np.stack([audio,audio*.94],axis=1)
wav=OUT/'soundtrack.wav'
with wave.open(str(wav),'wb') as f:
    f.setnchannels(2);f.setsampwidth(2);f.setframerate(rate);f.writeframes((stereo*32767).astype('<i2').tobytes())
ffmpeg=imageio_ffmpeg.get_ffmpeg_exe()
cmd=[ffmpeg,'-y','-loglevel','error','-f','rawvideo','-vcodec','rawvideo','-pix_fmt','rgb24','-s',f'{W}x{H}','-r',str(FPS),'-i','-','-i',str(wav),'-c:v','libx264','-preset','medium','-crf','20','-pix_fmt','yuv420p','-c:a','aac','-b:a','160k','-map_metadata','-1','-metadata','title=Agent Wiki — a little less starting over','-movflags','+faststart','-shortest',str(OUT/'agent-wiki-demo.mp4')]
proc=subprocess.Popen(cmd,stdin=subprocess.PIPE)
try:
    for n in range(FPS*DURATION):
        im=frame(n/FPS)
        proc.stdin.write(im.tobytes())
        if n in [72,216,384,528,696]: im.save(OUT/f'frame-{n:03}.png')
        if n==72: im.save(OUT/'poster.png')
finally:
    proc.stdin.close()
if proc.wait()!=0: raise SystemExit('Video encoding failed')
wav.unlink()
print(f'Rendered {DURATION}s, {W}x{H}, {FPS}fps')
