"""Filter the raw pool (generate_mime_pool.py) into mime_templates.json.

Drops trajectories MIME itself rendered badly: a single step > max(10% of
distance, 6px), lateral drift > 15%, overshoot > 8% or undershoot < -3%,
backtracking > 8%, landing > 2px off target, or fewer than 10 events.
Output rows: [angle_rad, distance_px, [[dt_ms, along/D, lateral/D], ...]].
"""
import json, math
data=json.load(open('/tmp/mime_pool_1000.json'))
T=[];why={}
for tr in data:
    sx,sy=tr['start'];ex,ey=tr['end'];dx,dy=ex-sx,ey-sy;D=math.hypot(dx,dy);ux,uy=dx/D,dy/D;nx,ny=-uy,ux
    P=tr['pts']
    if len(P)<10: why['few']=why.get('few',0)+1; continue
    jump=max(math.hypot(P[i][0]-P[i-1][0],P[i][1]-P[i-1][1]) for i in range(1,len(P)))
    s=[((x-sx)*ux+(y-sy)*uy)/D for x,y,t in P]; l=[((x-sx)*nx+(y-sy)*ny)/D for x,y,t in P]
    back=max(max(s[:j+1])-s[j] for j in range(len(s)))
    end=math.hypot(P[-1][0]-ex,P[-1][1]-ey)
    bad=[]
    if jump>max(0.10*D,6): bad.append('jump')
    if max(map(abs,l))>0.15: bad.append('lat')
    if max(s)>1.08 or min(s)<-0.03: bad.append('over')
    if back>0.08: bad.append('back')
    if end>2: bad.append('end')
    if bad:
        for b in bad: why[b]=why.get(b,0)+1
        continue
    t0=P[0][2]
    ev=[[round((P[i][2]-P[i-1][2])*1000,1), round(s[i],4), round(l[i],4)] for i in range(1,len(P))]
    T.append([round(math.atan2(dy,dx),3), round(D,1), ev])
print('kept',len(T),'of',len(data),'rejects',why)
import collections
bins=collections.Counter((int(math.log2(t[1])), int((t[0]+math.pi)/(math.pi/4))) for t in T)
print('dist bins (log2):',sorted(collections.Counter(int(math.log2(t[1])) for t in T).items()))
print('angle octants:',sorted(collections.Counter(int((t[0]+math.pi)/(math.pi/4))%8 for t in T).items()))
js=json.dumps(T,separators=(',',':')); open('/tmp/mk5/pool.json','w').write(js); print('bytes',len(js))
