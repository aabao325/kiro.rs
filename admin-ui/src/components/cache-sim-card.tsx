import { useState, useEffect } from 'react'
import { Database } from 'lucide-react'
import { toast } from 'sonner'
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { useCacheSimulation, useSetCacheSimulation } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

/**
 * 缓存 usage 模拟设置卡片
 *
 * Kiro 上游不支持 prompt caching，此开关在代理层模拟官方 API 的缓存 usage 字段
 * （cache_creation_input_tokens / cache_read_input_tokens / cache_creation）。
 * 仅当请求带 cache_control（5m/1h）时生效。
 */
export function CacheSimCard() {
  const { data, isLoading } = useCacheSimulation()
  const { mutate: save, isPending } = useSetCacheSimulation()

  // 本地编辑状态（比例以百分比 0-100 展示，提交时 /100）
  const [enabled, setEnabled] = useState(false)
  const [creationPct, setCreationPct] = useState(25)
  const [hitPct, setHitPct] = useState(70)
  const [cacheablePct, setCacheablePct] = useState(100)

  // 同步服务端数据到本地编辑状态
  useEffect(() => {
    if (data) {
      setEnabled(data.enabled)
      setCreationPct(Math.round(data.creationRatio * 100))
      setHitPct(Math.round(data.hitRatio * 100))
      setCacheablePct(Math.round(data.cacheableRatio * 100))
    }
  }, [data])

  const clampPct = (v: number) => Math.min(100, Math.max(0, Number.isFinite(v) ? v : 0))

  const handleSave = () => {
    save(
      {
        enabled,
        creationRatio: clampPct(creationPct) / 100,
        hitRatio: clampPct(hitPct) / 100,
        cacheableRatio: clampPct(cacheablePct) / 100,
      },
      {
        onSuccess: () => toast.success('缓存模拟设置已保存'),
        onError: (error) => toast.error(`保存失败: ${extractErrorMessage(error)}`),
      }
    )
  }

  return (
    <Card className="mt-6">
      <CardHeader>
        <div className="flex items-center justify-between">
          <div className="space-y-1.5">
            <CardTitle className="text-lg flex items-center gap-2">
              <Database className="h-5 w-5" />
              缓存 usage 模拟
            </CardTitle>
            <CardDescription>
              模拟官方 API 的 prompt caching usage 字段（仅对带 cache_control 的请求生效）。
              Kiro 上游无真实缓存，此处为模拟值。
            </CardDescription>
          </div>
          <Switch
            checked={enabled}
            onCheckedChange={setEnabled}
            disabled={isLoading || isPending}
            aria-label="启用缓存模拟"
          />
        </div>
      </CardHeader>
      <CardContent>
        <div className="grid grid-cols-1 sm:grid-cols-3 gap-4">
          <div className="space-y-1">
            <label className="text-sm font-medium">创建比例 (%)</label>
            <Input
              type="number"
              min={0}
              max={100}
              value={creationPct}
              disabled={!enabled || isLoading || isPending}
              onChange={(e) => setCreationPct(clampPct(e.target.valueAsNumber))}
            />
            <p className="text-xs text-muted-foreground">cache_creation 占可缓存基数比例</p>
          </div>
          <div className="space-y-1">
            <label className="text-sm font-medium">命中比例 (%)</label>
            <Input
              type="number"
              min={0}
              max={100}
              value={hitPct}
              disabled={!enabled || isLoading || isPending}
              onChange={(e) => setHitPct(clampPct(e.target.valueAsNumber))}
            />
            <p className="text-xs text-muted-foreground">cache_read 占可缓存基数比例</p>
          </div>
          <div className="space-y-1">
            <label className="text-sm font-medium">可缓存比例 (%)</label>
            <Input
              type="number"
              min={0}
              max={100}
              value={cacheablePct}
              disabled={!enabled || isLoading || isPending}
              onChange={(e) => setCacheablePct(clampPct(e.target.valueAsNumber))}
            />
            <p className="text-xs text-muted-foreground">输入中算作可缓存基数的比例</p>
          </div>
        </div>
        <div className="mt-4 flex justify-end">
          <Button onClick={handleSave} disabled={isLoading || isPending} size="sm">
            {isPending ? '保存中...' : '保存'}
          </Button>
        </div>
      </CardContent>
    </Card>
  )
}
