# assets

`muser-console-social-card.png` is the 1200×630 social preview image. It is
generated deterministically — do not edit it by hand:

```sh
python3 scripts/generate_social_card.py           # regenerate
python3 scripts/generate_social_card.py --check   # verify committed copy
```

The GitHub social preview is configured manually: repo → Settings → General
→ Social preview → upload this file.

`muser-onboarding-and-remote-prefill.png` is a real frame from the shared
onboarding demo linked in the root README. The canonical H.264 video lives in
the main `muser` repository at
[`docs/assets/muser-onboarding-and-remote-prefill.mp4`](https://github.com/High-Performance-AI-Lab/muser/blob/main/docs/assets/muser-onboarding-and-remote-prefill.mp4),
so this companion repository does not duplicate the video binary.

A real dashboard screenshot belongs here too — capture it from a running
console (see the README quick start); do not synthesize one.
