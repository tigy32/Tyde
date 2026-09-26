import {mkdir, readFile, writeFile, mkdtemp, rm} from 'node:fs/promises';
import {join} from 'node:path';
import {tmpdir} from 'node:os';

export async function writeScreenshot(output, filename, png) {
    if (!/^shell-shot-[a-z0-9-]+\.png$/.test(filename) || !Buffer.isBuffer(png) ||
        png.length > 8_000_000 || !png.subarray(0,8).equals(Buffer.from([137,80,78,71,13,10,26,10]))) {
        throw new Error('SCREENSHOT_OUTPUT_INVALID');
    }
    const directory=join(output,'shell-images');
    await mkdir(directory,{recursive:true});
    await writeFile(join(directory,filename),png,{mode:0o600});
}

// Exercise the same first-write path as the device runner, not just stat/access.
export async function preflightScreenshotOutput(png) {
    const root=await mkdtemp(join(tmpdir(),'tyde-device-output-'));
    try {
        const output=join(root,'previously-absent','device-output');
        const filename='shell-shot-output-preflight.png';
        await writeScreenshot(output,filename,png);
        if(!(await readFile(join(output,'shell-images',filename))).equals(png))throw new Error('SCREENSHOT_OUTPUT_ROUNDTRIP_FAILED');
        return {status:'PASS',initialDirectoryAbsent:true,created:true,payloadRoundtrip:true,bytes:png.length};
    } finally {await rm(root,{recursive:true,force:true});}
}
