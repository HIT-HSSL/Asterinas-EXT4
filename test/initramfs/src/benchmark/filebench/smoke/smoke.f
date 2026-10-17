set $dir=/ext2/filebench-smoke
set $filesize=1m
set $iosize=4k
set $runtime=3

define file name=smokefile,path=$dir,size=$filesize,prealloc

define process name=smokewriter,instances=1
{
  thread name=smokewriterthread,memsize=1m,instances=1
  {
    flowop write name=write-file,filename=smokefile,random,iosize=$iosize
    flowop fsync name=sync-file
  }
}

run $runtime
