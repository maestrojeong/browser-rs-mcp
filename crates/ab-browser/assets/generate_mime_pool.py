"""Generate the raw MIME-mouse trajectory pool behind mime_templates.json.

Run from a checkout of https://github.com/4LAU/MIME-mouse (Apache-2.0) with its
release checkpoints downloaded (`python setup_data.py`), adjusting the
sys.path line below. Writes /tmp/mime_pool_1000.json: 1000 trajectories over
random directions and log-uniform 40-1000px distances on a 1920x1080 screen.
"""
import json, math, random, sys, time
sys.path.insert(0, '/tmp/mime-mouse-check')
import torch; torch.set_num_threads(2)
from generate import generate
random.seed(42); N=1000; out=[]; t0=time.time(); seed=1000
while len(out)<N and seed<3000:
    seed+=1
    D=math.exp(random.uniform(math.log(40), math.log(1000))); ang=random.uniform(-math.pi, math.pi)
    for _ in range(50):
        x0=random.uniform(10,1910); y0=random.uniform(10,1070)
        x1=x0+D*math.cos(ang); y1=y0+D*math.sin(ang)
        if 10<=x1<=1910 and 10<=y1<=1070: break
    else: continue
    try: tr=generate(x0,y0,x1,y1,n=1,seed=seed)
    except Exception: continue
    if len(tr)<4: continue
    out.append({"start":[round(x0),round(y0)],"end":[round(x1),round(y1)],"pts":[[float(p[0]),float(p[1]),float(p[2])] for p in tr]})
    if len(out)%100==0: print(len(out), round(time.time()-t0,1), flush=True)
json.dump(out, open('/tmp/mime_pool_1000.json','w'))
print('DONE', len(out), round(time.time()-t0,1))
