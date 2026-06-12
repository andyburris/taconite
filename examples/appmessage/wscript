import os.path
import os
import glob

top = '.'
out = 'build'

def options(ctx):
    ctx.load('pebble_sdk')

def configure(ctx):
    ctx.load('pebble_sdk')

def build(ctx):
    ctx.load('pebble_sdk')

    build_worker = os.path.exists('worker_src')
    binaries = []

    for p in ctx.env.TARGET_PLATFORMS:
        ctx.set_env(ctx.all_envs[p])
        ctx.set_group(ctx.env.PLATFORM_NAME)
        app_elf='{}/pebble-app.elf'.format(ctx.env.BUILD_DIR)

        #Wish this could be in configure, but LINKFLAGS gets reset between aplite & basalt
        os.chdir('build')
        ctx.env.LINKFLAGS.append('-Wl,--allow-multiple-definition')
        # GCC 14 links unwind-arm.o from libgcc even with panic=abort; stub out
        # the ARM EHABI exception-table boundaries since Pebble has no exceptions.
        ctx.env.LINKFLAGS.append('-Wl,--defsym=__exidx_start=0')
        ctx.env.LINKFLAGS.append('-Wl,--defsym=__exidx_end=0')
        ctx.env.LINKFLAGS += glob.glob('../target/thumbv7m-none-eabi/release/deps/*.o')
        os.chdir('..')
        
        ctx.pbl_program(source=ctx.path.ant_glob('target/thumbv7m-none-eabi/release/deps/*.o'),
        target=app_elf)

        if build_worker:
            worker_elf='{}/pebble-worker.elf'.format(ctx.env.BUILD_DIR)
            binaries.append({'platform': p, 'app_elf': app_elf, 'worker_elf': worker_elf})
            ctx.pbl_worker(source=ctx.path.ant_glob('worker_src/**/*.c'),
            target=worker_elf)
        else:
            binaries.append({'platform': p, 'app_elf': app_elf})

    ctx.set_group('bundle')
    ctx.pbl_bundle(binaries=binaries, js=ctx.path.ant_glob('src/js/**/*.js'))
